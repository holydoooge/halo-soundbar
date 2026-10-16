//! Embed Windows resources into the executable.
//!
//! `WindowVisuals::icon()` only sets the *runtime* window icon, which covers the
//! taskbar and Alt-Tab. Explorer reads the icon from the PE resources instead,
//! so without this the binary shows a blank page in a folder listing — and it
//! also had no version information in its file properties.

fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        // Multi-resolution (16/24/32/48/64/128/256), so Explorer, the taskbar
        // and Alt-Tab all pick an appropriately sized frame.
        res.set_icon("assets/app.ico");
        res.set("ProductName", "Halo Soundbar");
        res.set("FileDescription", "Halo Soundbar control panel");
        res.set(
            "Comments",
            "Local USB control for the EDIFIER Halo Soundbar. No network features.",
        );
        res.set("LegalCopyright", "Reverse-engineered for interoperability");
        if let Err(e) = res.compile() {
            // A missing resource compiler must not break the build; the app is
            // fully functional without the shell icon.
            println!("cargo:warning=could not embed Windows resources: {e}");
        }
    }
}
