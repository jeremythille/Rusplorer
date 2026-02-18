fn main() {
    #[cfg(windows)]
    {
        winres::WindowsResource::new()
            .set_icon("logo/Rustplorer-logo.ico")
            .compile()
            .expect("Failed to compile Windows resources");
    }
}
