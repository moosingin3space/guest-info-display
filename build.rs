fn main() {
    pkg_config::probe_library("xcb").unwrap();
    pkg_config::probe_library("xkbcommon").unwrap();
    pkg_config::probe_library("xkbcommon-x11").unwrap();
}
