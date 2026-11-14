# Face confirmation for SSH (secure remote use)

FROGGER never uses the camera of an SSH *server*: that camera sees whoever
sits in front of the server, not the person logged in remotely. Streaming
camera images from the client to the server would be insecure (images can be
injected). Instead the face is checked where the person actually sits – on
the **client** – and the server only receives a normal SSH signature.

```
 laptop (FROGGER, IR camera)                         server (no FROGGER needed)
 ┌──────────────────────────────┐   SSH signature   ┌───────────────────────┐
 │ ssh-agent ── key added -c ──►│ ────────────────► │ sshd: public key auth │
 │   │ confirm?                 │                   │ sudo: pam_ssh_agent_  │
 │   ▼                          │ ◄── forwarded ─── │       auth (optional) │
 │ frogger-askpass → froggerd   │      agent        └───────────────────────┘
 │   (IR + blink, local only)   │
 └──────────────────────────────┘
```

## Client setup (once per user, no root)

```bash
frogger ssh enable
```

Log out and back in, then add your keys with the *confirm* constraint:

```bash
ssh-add -c ~/.ssh/id_ed25519
```

From now on every use of that key – logging in to a server, `git push`,
signing – shows a notification "SSH key requested: look at the camera and
blink". The signature is only produced after successful face authentication
on the laptop (same rules as login: IR camera, blink, no remote session).

* `frogger ssh status` / `frogger ssh disable` show or undo the setup.
* Passphrase prompts are passed on to the desktop's askpass program
  (ksshaskpass, gnome-ssh-askpass, …); ssh in a terminal keeps prompting in
  the terminal.
* Technically: `SSH_ASKPASS` is set via `~/.config/environment.d`, and
  `SSH_ASKPASS_REQUIRE=force` only for `ssh-agent.service` and
  `gcr-ssh-agent.service` (systemd user drop-ins), so the agent asks even on
  Wayland without an X11 `DISPLAY`. Requires OpenSSH ≥ 8.4.

## sudo on the server with your face

On the **server**, `pam_ssh_agent_auth` lets `sudo` authenticate with the
forwarded agent – which in turn asks for your face on the laptop.

Debian/Ubuntu server:

```bash
sudo apt install libpam-ssh-agent-auth
```

`/etc/pam.d/sudo` (above `@include common-auth`):

```
auth sufficient pam_ssh_agent_auth.so file=/etc/security/authorized_keys
```

Put the users' public keys into `/etc/security/authorized_keys` (owned by
root, not writable by the users – otherwise a user could authorise any key)
and keep the agent socket for sudo:

```
# /etc/sudoers.d/ssh-agent-auth
Defaults env_keep += "SSH_AUTH_SOCK"
```

Connect with agent forwarding (`ssh -A server`, or `ForwardAgent yes` for
that host only). `sudo` on the server now triggers the face confirmation on
your laptop.

## Security notes

* **Agent forwarding is normally risky**: root on the server can use a
  forwarded agent while you are connected. With `ssh-add -c` and FROGGER
  every such use needs your face on the laptop, so misuse is visible and
  can be refused – forward the agent only to hosts you need.
* For keys that must not be copyable, keep them in the TPM
  ([ssh-tpm-agent](https://github.com/Foxboron/ssh-tpm-agent)) or on a FIDO2
  token; the confirmation works the same way.
* The notification shows the key, not the target host. Deny requests you
  did not trigger yourself.
