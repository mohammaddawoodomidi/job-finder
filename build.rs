fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Job Finder");
        res.set("FileDescription", "Job Finder");
        let _ = res.compile(); // a missing resource compiler must never break the build
    }
}
