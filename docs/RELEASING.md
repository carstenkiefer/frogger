# Releasing

## A release

Before tagging, in a pull request: `version` in `Cargo.toml` (the base of
development builds, which sort before the release) and `_tag` in
`packaging/arch/PKGBUILD`. Then:

```bash
git checkout main && git pull
git tag -a v0.3.0-alpha -m "FROGGER 0.3.0-alpha"
git push origin v0.3.0-alpha
```

CI builds and tests the packages (amd64, arm64, Arch) and publishes a GitHub
release; tags with a suffix (`-alpha`, `-rc1`) become pre-releases. Do not
create the release in the web interface – CI creates it.

Afterwards the workflow **Package repository** updates the APT/DNF
repository on GitHub Pages with the packages of the newest FROGGER release
and the newest [frogger-voices](https://github.com/carstenkiefer/frogger-voices)
release. It also runs daily (new voice releases) and can be started by hand
under *Actions → Package repository → Run workflow*.

## One-time setup of the package repository

1. **Signing key** (only for the repository, no passphrase, on a trusted
   machine):

   ```bash
   export GNUPGHOME=$(mktemp -d)
   gpg --batch --passphrase '' --quick-gen-key "FROGGER packages <carstenkiefer@users.noreply.github.com>" rsa4096 sign never
   gpg --armor --export-secret-keys > frogger-repo-signing.key
   gpg --fingerprint
   ```

   Keep `frogger-repo-signing.key` in a safe place (password manager,
   offline backup): with a new key every user has to import the key again.
2. **Secret:** *Settings → Secrets and variables → Actions → New repository
   secret*, name `REPO_SIGNING_KEY`, content of `frogger-repo-signing.key`.
   Then delete the temporary key ring: `rm -rf "$GNUPGHOME"`.
3. **Pages:** *Settings → Pages → Build and deployment → Source: GitHub
   Actions*.
4. Start *Actions → Package repository → Run workflow* once; the repository
   is then at https://carstenkiefer.github.io/frogger/ (the page shows the key
   fingerprint).

`packaging/build-repo.sh <package dir> <out dir>` builds the same repository
locally (needs `apt-utils`, `createrepo-c` and the key in the GnuPG keyring).
