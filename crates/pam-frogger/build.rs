fn main() {
    // Only the cdylib needs libpam; resolved at load time inside the PAM stack.
    println!("cargo:rustc-link-lib=dylib=pam");
}
