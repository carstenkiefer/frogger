// Tests the extension logic with a mock of GNOME Shell's ShellUserVerifier
// (the parts of js/gdm/util.js it relies on, GNOME 46–50).
//   node gnome/test/extension.test.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const src = fs.readFileSync(path.join(here, '../frogger@carstenkiefer.github.io/extension.js'), 'utf8');

// ---- mocks for the GNOME modules the extension imports ----
const mocks = `
const timers = new Map(); let nextTimer = 1;
const GLib = {
    PRIORITY_DEFAULT: 0, SOURCE_REMOVE: false,
    timeout_add(_p, _ms, cb) { const id = nextTimer++; timers.set(id, cb); return id; },
    source_remove(id) { timers.delete(id); },
};
globalThis.__timers = timers;
globalThis.logError = (e, msg) => { throw new Error(msg + ': ' + e); };
class Extension {}
class InjectionManager {
    #saved = [];
    overrideMethod(proto, name, create) {
        const original = proto[name];
        this.#saved.push([proto, name, original]);
        proto[name] = create(original);
    }
    clear() { for (const [p, n, o] of this.#saved.reverse()) p[n] = o; this.#saved = []; }
}
const MessageType = {NONE: 0, HINT: 1, INFO: 2, ERROR: 3};
class ShellUserVerifier {
    constructor() {
        this._activeServices = new Set(); this._unavailableServices = new Set();
        this._userName = 'alice'; this._userVerifier = {};
        this._cancellable = {cancelled: false, is_cancelled() { return this.cancelled; }};
        this.started = []; this.messages = []; this.failed = [];
    }
    _beginVerification() { this._startService('gdm-password'); }
    async _startService(s) { this.started.push(s); this._activeServices.add(s); }
    _onInfo(_c, s, i) { if (s === 'gdm-password') this._queueMessage(s, i, MessageType.INFO); }
    _queueMessage(s, m, t) { this.messages.push([s, m, t]); }
    _filterServiceMessages() {}
    async _verificationFailed(s, retry) { this.failed.push([s, retry]); }
    _onReset() { this.wasReset = true; }
    clear() { this.cleared = true; }
    // like util.js: a background service that stops is retried via _verificationFailed
    _onConversationStopped(_c, s) {
        this._activeServices.delete(s);
        if (this._unavailableServices.has(s)) return;
        this._verificationFailed(s, true);
    }
}
const GdmUtil = {ShellUserVerifier, MessageType};
export {ShellUserVerifier};
`;
const patched = src
    .replace(/^import GLib from .*$/m, '')
    .replace(/^import \{Extension, InjectionManager\} from .*$/m, '')
    .replace(/^import \* as GdmUtil from .*$/m, mocks);
assert.notEqual(patched, src);
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'frogger-ext-'));
const file = path.join(tmp, 'extension.mjs');
fs.writeFileSync(file, patched);
const mod = await import(pathToFileURL(file));
const {ShellUserVerifier} = mod;
const Ext = mod.default;
const fire = () => { for (const [id, cb] of [...globalThis.__timers]) { globalThis.__timers.delete(id); cb(); } };

const proto = ShellUserVerifier.prototype;
const originals = Object.fromEntries(['_beginVerification', '_onInfo', '_verificationFailed', '_onReset', 'clear'].map(n => [n, proto[n]]));
const ext = new Ext();
ext.enable();

// 1. The face service starts together with the password.
let v = new ShellUserVerifier();
v._beginVerification();
assert.deepEqual(v.started, ['gdm-password', 'gdm-frogger']);

// 2. Face messages become hints, password messages are untouched.
v._onInfo(null, 'gdm-frogger', 'Look at the camera');
v._onInfo(null, 'gdm-password', 'Password:');
assert.deepEqual(v.messages, [['gdm-frogger', 'Look at the camera', 1], ['gdm-password', 'Password:', 2]]);

// 3. A failed face attempt is retried twice, then left alone; it never
//    reaches the shell's own failure handling (no reset, no failed password).
for (let i = 0; i < 3; i++) {
    v._onConversationStopped(null, 'gdm-frogger');
    fire();
}
assert.deepEqual(v.started.filter(s => s === 'gdm-frogger').length, 3);
assert.equal(globalThis.__timers.size, 0);
assert.deepEqual(v.failed, []);

// 4. Password failures still go to the shell.
await v._verificationFailed('gdm-password', true);
assert.deepEqual(v.failed, [['gdm-password', true]]);

// 5. A new prompt (reset) allows new face attempts.
v._onReset();
assert.ok(v.wasReset);
v._onConversationStopped(null, 'gdm-frogger');
fire();
assert.equal(v.started.filter(s => s === 'gdm-frogger').length, 4);

// 6. No restart after cancel, and a pending retry is dropped on clear().
v._onConversationStopped(null, 'gdm-frogger');
v._cancellable.cancelled = true;
fire();
assert.equal(v.started.filter(s => s === 'gdm-frogger').length, 4);
v = new ShellUserVerifier();
v._onConversationStopped(null, 'gdm-frogger');
assert.equal(globalThis.__timers.size, 1);
v.clear();
assert.equal(globalThis.__timers.size, 0);

// 7. Not started without a user (login screen user list), or when the PAM
//    service is missing (GDM reported it unavailable).
v = new ShellUserVerifier(); v._userName = null; v._beginVerification();
assert.deepEqual(v.started, ['gdm-password']);
v = new ShellUserVerifier(); v._unavailableServices.add('gdm-frogger'); v._beginVerification();
assert.deepEqual(v.started, ['gdm-password']);

// 8. disable() restores the shell.
ext.disable();
for (const [n, f] of Object.entries(originals))
    assert.equal(proto[n], f, n);

fs.rmSync(tmp, {recursive: true});
console.log('extension tests passed');
