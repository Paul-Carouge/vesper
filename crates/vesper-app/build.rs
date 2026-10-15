fn main() {
    // Pin the widget style so the look does not depend on which toolkits are
    // installed (only `Palette.color-scheme` is used from std-widgets).
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("failed to compile the Slint UI");
}
