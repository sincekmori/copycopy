// GNOME Shell backend for the copycopy Rust crate.
//
// GNOME Wayland offers no unprivileged way to observe global keys or to read
// the clipboard from a background process, so this extension runs inside the
// compositor instead: it watches clipboard *owner changes*, detects the
// "two explicit copies in quick succession" gesture (the Ctrl+C+C equivalent),
// reads the freshly copied content with St.Clipboard, and hands it to the host
// application over D-Bus.
//
// Security: clipboard contents are NEVER broadcast on the session bus (a
// broadcast could be read by any process on the bus). Only a metadata-only
// `CaptureReady(serial)` signal is emitted; the content travels in the
// unicast reply of `TakeCapture(serial)`, which is one-shot and expires after
// a short TTL.
//
// Maintenance — GNOME releases a new major every March and September, and
// metadata.json's `shell-version` has NO range/wildcard syntax: majors must
// be listed explicitly, or GNOME disables the extension at login on that
// version. On each new GNOME release: append the major to `shell-version`,
// bump `version`, and cut a patch release of the crate — the installer
// overwrites older installed copies by version comparison, so consumers only
// need the crate update. Declared 45-50 (45+ share this ESM entry point and
// every API used here: Meta.Selection owner-changed, St.Clipboard
// get_mimetypes/get_content, global.display.focus_window,
// Gio.DBusExportedObject); verified on a real GNOME 46 (Ubuntu 24.04)
// session.

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const DBUS_PATH = '/io/github/sincekmori/copycopy';
const DBUS_IFACE = 'io.github.sincekmori.copycopy';

// One explicit copy fires 1-3 `owner-changed` events in a ~1 ms burst;
// events closer than this to the previous one are coalesced into one copy.
const COALESCE_MS = 50;
// Max gap between two coalesced copies to count as a double copy. Mirrors the
// key-based detector's `double_tap_window` default on the other platforms.
const DOUBLE_COPY_MS = 400;
// Reading immediately after `owner-changed` can return nothing; wait for the
// burst to go quiet plus this long before reading.
const SETTLE_MS = 150;
// Right after a double copy, the offered MIME list can be transiently
// impoverished (e.g. Nautilus momentarily drops text/uri-list). If the offer
// looks poorer than at detection time, wait this long and re-read once.
const REREAD_MS = 150;
// A pending capture not taken via TakeCapture within this TTL is dropped.
const TAKE_TTL_MS = 5000;

const PLAIN_TEXT_MIMES = ['text/plain;charset=utf-8', 'UTF8_STRING', 'text/plain'];

const IFACE_XML = `
<node>
  <interface name="${DBUS_IFACE}">
    <method name="TakeCapture">
      <arg type="u" direction="in" name="serial"/>
      <arg type="s" direction="out" name="kind"/>
      <arg type="s" direction="out" name="mime"/>
      <arg type="ay" direction="out" name="data"/>
      <arg type="s" direction="out" name="plain"/>
      <arg type="s" direction="out" name="app_name"/>
      <arg type="s" direction="out" name="wm_class"/>
      <arg type="s" direction="out" name="title"/>
      <arg type="u" direction="out" name="pid"/>
    </method>
    <method name="GetVersion">
      <arg type="u" direction="out" name="version"/>
    </method>
    <signal name="CaptureReady">
      <arg type="u" name="serial"/>
    </signal>
  </interface>
</node>`;

export default class CopycopyExtension extends Extension {
    enable() {
        this._serial = 0;
        this._pending = new Map(); // serial -> {payload, fg, timeoutId}
        this._lastEventMs = null; // last owner-changed, for burst coalescing
        this._prevCopyMs = null; // previous coalesced copy, for double detection
        this._captureArmed = false;
        this._settleId = 0;
        this._rereadId = 0;
        this._rankAtDetect = 0;
        this._fg = null;

        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbus.export(Gio.DBus.session, DBUS_PATH);

        this._selection = global.display.get_selection();
        this._ownerChangedId = this._selection.connect(
            'owner-changed', (_selection, selectionType, _owner) => {
                if (selectionType === Meta.SelectionType.SELECTION_CLIPBOARD)
                    this._onClipboardOwnerChanged();
            });
    }

    disable() {
        if (this._selection && this._ownerChangedId)
            this._selection.disconnect(this._ownerChangedId);
        this._ownerChangedId = 0;
        this._selection = null;
        if (this._settleId)
            GLib.source_remove(this._settleId);
        this._settleId = 0;
        if (this._rereadId)
            GLib.source_remove(this._rereadId);
        this._rereadId = 0;
        if (this._pending) {
            for (const entry of this._pending.values())
                GLib.source_remove(entry.timeoutId);
            this._pending.clear();
        }
        this._pending = null;
        if (this._dbus)
            this._dbus.unexport();
        this._dbus = null;
        this._fg = null;
    }

    _onClipboardOwnerChanged() {
        const now = GLib.get_monotonic_time() / 1000;

        if (this._captureArmed) {
            // The second copy's burst may still be arriving; each event pushes
            // the settle timer back so we read only once the burst is quiet.
            this._lastEventMs = now;
            this._scheduleSettle();
            return;
        }

        if (this._lastEventMs !== null && now - this._lastEventMs <= COALESCE_MS) {
            this._lastEventMs = now; // same burst: absorb
            return;
        }
        this._lastEventMs = now;

        // Start of a new coalesced copy.
        if (this._prevCopyMs !== null && now - this._prevCopyMs <= DOUBLE_COPY_MS) {
            this._prevCopyMs = null; // reset: a third copy starts a new sequence
            this._armCapture();
        } else {
            this._prevCopyMs = now;
        }
    }

    _armCapture() {
        this._captureArmed = true;
        // Snapshot the foreground window now; by the time the clipboard
        // settles, focus could in principle have moved.
        this._fg = this._snapshotForeground();
        this._rankAtDetect = this._rankOf(this._clipboardMimetypes());
        this._scheduleSettle();
    }

    _scheduleSettle() {
        // Cancel BOTH timers: with a re-read pending, a new owner-changed
        // must restart the settle wait instead of adding a second timer —
        // otherwise both would fire and publish the same gesture twice.
        if (this._settleId)
            GLib.source_remove(this._settleId);
        if (this._rereadId) {
            GLib.source_remove(this._rereadId);
            this._rereadId = 0;
        }
        this._settleId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, SETTLE_MS, () => {
            this._settleId = 0;
            this._readAndPublish(1);
            return GLib.SOURCE_REMOVE;
        });
    }

    _readAndPublish(rereadsLeft) {
        const mimes = this._clipboardMimetypes();
        if (rereadsLeft > 0 && this._rankOf(mimes) < this._rankAtDetect) {
            this._rereadId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, REREAD_MS, () => {
                this._rereadId = 0;
                this._readAndPublish(rereadsLeft - 1);
                return GLib.SOURCE_REMOVE;
            });
            return;
        }
        this._captureArmed = false;
        this._readByPriority(mimes, payload => this._publish(payload));
    }

    _clipboardMimetypes() {
        return St.Clipboard.get_default().get_mimetypes(St.ClipboardType.CLIPBOARD) ?? [];
    }

    // Content-kind rank of a MIME offer: files > image > rich > plain > none.
    _rankOf(mimes) {
        if (mimes.includes('text/uri-list') || mimes.includes('x-special/gnome-copied-files'))
            return 4;
        if (mimes.includes('image/png'))
            return 3;
        if (mimes.includes('text/html'))
            return 2;
        if (PLAIN_TEXT_MIMES.some(m => mimes.includes(m)))
            return 1;
        return 0;
    }

    _readByPriority(mimes, callback) {
        // Prefer a MIME with an explicit encoding: Firefox puts broken escape
        // sequences (a literal six-character "\\u5831" instead of the actual
        // character) into bare text/plain, so that one is tried last.
        const plainMime = PLAIN_TEXT_MIMES.find(m => mimes.includes(m)) ?? null;

        const attempts = [];
        if (mimes.includes('text/uri-list'))
            attempts.push({kind: 'files', mime: 'text/uri-list'});
        else if (mimes.includes('x-special/gnome-copied-files'))
            attempts.push({kind: 'files', mime: 'x-special/gnome-copied-files'});
        if (mimes.includes('image/png'))
            attempts.push({kind: 'image', mime: 'image/png'});
        if (mimes.includes('text/html'))
            attempts.push({kind: 'rich', mime: 'text/html'});
        if (plainMime)
            attempts.push({kind: 'text', mime: plainMime});

        this._tryAttempts(attempts, plainMime, callback);
    }

    // Read `attempts` in order until one returns non-empty content; fall back
    // to an "empty" payload when none does.
    _tryAttempts(attempts, plainMime, callback) {
        if (attempts.length === 0) {
            callback({kind: 'empty', mime: '', data: new Uint8Array(0), plain: ''});
            return;
        }
        const [attempt, ...rest] = attempts;
        this._getBytes(attempt.mime, data => {
            if (data === null || data.length === 0) {
                this._tryAttempts(rest, plainMime, callback);
                return;
            }
            if (attempt.kind === 'rich' && plainMime) {
                // Also carry the plain-text fallback alongside the markup.
                this._getBytes(plainMime, plainData => {
                    const plain = plainData ? new TextDecoder().decode(plainData) : '';
                    callback({kind: 'rich', mime: attempt.mime, data, plain});
                });
                return;
            }
            callback({kind: attempt.kind, mime: attempt.mime, data, plain: ''});
        });
    }

    _getBytes(mime, callback) {
        St.Clipboard.get_default().get_content(
            St.ClipboardType.CLIPBOARD, mime, (_clipboard, bytes) => {
                callback(bytes ? bytes.get_data() ?? null : null);
            });
    }

    _snapshotForeground() {
        const win = global.display.focus_window;
        if (!win)
            return {appName: '', wmClass: '', title: '', pid: 0};
        const app = Shell.WindowTracker.get_default().get_window_app(win);
        return {
            appName: app?.get_name() ?? '',
            wmClass: win.get_wm_class() ?? '',
            title: win.get_title() ?? '',
            pid: Math.max(win.get_pid() ?? 0, 0),
        };
    }

    _publish(payload) {
        this._serial = ((this._serial + 1) >>> 0) || 1;
        const serial = this._serial;
        const timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, TAKE_TTL_MS, () => {
            this._pending.delete(serial);
            return GLib.SOURCE_REMOVE;
        });
        this._pending.set(serial, {payload, fg: this._fg, timeoutId});
        this._dbus.emit_signal('CaptureReady', new GLib.Variant('(u)', [serial]));
    }

    // --- D-Bus methods ---

    TakeCapture(serial) {
        const entry = this._pending.get(serial);
        if (!entry)
            return ['expired', '', [], '', '', '', '', 0];
        GLib.source_remove(entry.timeoutId);
        this._pending.delete(serial);
        const {payload, fg} = entry;
        return [
            payload.kind, payload.mime, payload.data, payload.plain,
            fg?.appName ?? '', fg?.wmClass ?? '', fg?.title ?? '', fg?.pid ?? 0,
        ];
    }

    GetVersion() {
        return this.metadata.version ?? 0;
    }
}
