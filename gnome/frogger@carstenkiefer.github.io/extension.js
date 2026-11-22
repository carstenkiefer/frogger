// SPDX-License-Identifier: MIT OR Apache-2.0
//
// FROGGER face login next to the password field.
//
// GNOME Shell's user verifier (js/gdm/util.js, used by the login screen and
// the lock screen) runs the password service and, if present, the
// fingerprint service in parallel. This extension adds a third service,
// the PAM service "gdm-frogger" (/etc/pam.d/gdm-frogger, written by
// `frogger gnome enable`), so face recognition runs while the password
// field stays usable. Whatever finishes first wins; typing the password
// cancels the face service and the daemon stops the camera at once.
//
// Everything security-relevant happens in PAM and in the FROGGER daemon;
// this extension only starts the service and shows its hints. If it breaks,
// the password still works.

import GLib from 'gi://GLib';
import {Extension, InjectionManager} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as GdmUtil from 'resource:///org/gnome/shell/gdm/util.js';

const SERVICE = 'gdm-frogger';
// A failed attempt (nobody looking, not recognised) is retried this often
// per prompt; after that the password remains. Each attempt ends after a few
// seconds without a face, so the camera does not stay on.
const MAX_RETRIES = 2;
const RETRY_DELAY_MS = 1500;

function startFace(verifier) {
    try {
        if (!verifier._userName || !verifier._userVerifier)
            return;
        if (verifier._activeServices.has(SERVICE) ||
            verifier._unavailableServices?.has(SERVICE))
            return;
        // _startService handles its own errors for background services.
        verifier._startService(SERVICE);
    } catch (e) {
        logError(e, 'FROGGER: could not start face authentication');
    }
}

function cancelRetry(verifier) {
    if (verifier._froggerRetryId) {
        GLib.source_remove(verifier._froggerRetryId);
        verifier._froggerRetryId = 0;
    }
}

function scheduleRetry(verifier) {
    cancelRetry(verifier);
    verifier._froggerRetries = (verifier._froggerRetries ?? 0) + 1;
    if (verifier._froggerRetries > MAX_RETRIES)
        return;
    const cancellable = verifier._cancellable;
    verifier._froggerRetryId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, RETRY_DELAY_MS, () => {
        verifier._froggerRetryId = 0;
        if (cancellable && !cancellable.is_cancelled())
            startFace(verifier);
        return GLib.SOURCE_REMOVE;
    });
}

export default class FroggerExtension extends Extension {
    enable() {
        this._injections = new InjectionManager();
        const proto = GdmUtil.ShellUserVerifier.prototype;

        // Start the face service together with password and fingerprint.
        this._injections.overrideMethod(proto, '_beginVerification', original =>
            function (...args) {
                const result = original.apply(this, args);
                startFace(this);
                return result;
            });

        // Messages of the face service ("look at the camera", "blink") are
        // shown as hints below the password field, like fingerprint hints.
        this._injections.overrideMethod(proto, '_onInfo', original =>
            function (client, serviceName, info) {
                if (serviceName !== SERVICE)
                    return original.call(this, client, serviceName, info);
                this._queueMessage(serviceName, info, GdmUtil.MessageType.HINT);
                return undefined;
            });

        // A failed face attempt must never reset the prompt or count as a
        // failed password: retry a few times, then leave the password.
        this._injections.overrideMethod(proto, '_verificationFailed', original =>
            function (serviceName, shouldRetry) {
                if (serviceName !== SERVICE)
                    return original.call(this, serviceName, shouldRetry);
                this._filterServiceMessages(serviceName, GdmUtil.MessageType.HINT);
                if (shouldRetry)
                    scheduleRetry(this);
                return Promise.resolve();
            });

        // A new prompt (reset after a wrong password, new user) starts over.
        this._injections.overrideMethod(proto, '_onReset', original =>
            function (...args) {
                cancelRetry(this);
                this._froggerRetries = 0;
                return original.apply(this, args);
            });

        this._injections.overrideMethod(proto, 'clear', original =>
            function (...args) {
                cancelRetry(this);
                return original.apply(this, args);
            });
    }

    disable() {
        // Also called when the screen locks if the extension were not
        // allowed in "unlock-dialog"; it is, so this runs only when the
        // user really disables it.
        this._injections?.clear();
        this._injections = null;
    }
}
