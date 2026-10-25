//! Regression tests for the shared release-target interface.
#[cfg(test)]
mod tests {
    use crate::platform::target::*;

    #[test]
    fn a_program_is_named_the_way_this_system_names_programs() {
        assert_eq!(
            exe_name("sidevoice-connector"),
            format!("sidevoice-connector{}", std::env::consts::EXE_SUFFIX)
        );
        assert!(exe_name("x").starts_with('x'));
        assert_eq!(exe_name("x").ends_with(".exe"), cfg!(windows));
    }

    #[test]
    fn this_build_names_its_release_target_by_its_system_and_architecture() {
        let name = build_target().map(|target| target.name);
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        match name {
            Some(name) => {
                let (name_os, name_arch) = name.split_once('-').unwrap();
                assert_eq!(name_os, os);
                assert_eq!(name_arch, arch);
            }
            None => assert!(![
                "macos-aarch64",
                "linux-x86_64",
                "linux-aarch64",
                "windows-x86_64"
            ]
            .contains(&format!("{os}-{arch}").as_str())),
        }
    }
}
