fn main() {
    // One compile call: host.slint imports setup.slint, so the generated file
    // exports both HostWindow and SetupDialog. (Each `compile_with_config`
    // call would overwrite the SLINT_INCLUDE_GENERATED env var, so two calls
    // would silently drop one of the two generated files.)
    slint_build::compile("ui/host.slint").unwrap();
}
