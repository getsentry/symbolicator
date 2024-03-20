use crate::{interface::JvmFrame, ProguardService};

impl ProguardService {
    fn is_valid_path(abs_path: &str) -> Option<&str> {
        if abs_path.contains('$') {
            return None;
        }
        let (before, _) = abs_path.rsplit_once('.')?;
        Some(before)
    }

    fn build_source_file_name(frame: &JvmFrame) -> String {
        let abs_path = frame.abs_path.as_deref();
        let module = &frame.module;
        let mut source_file_name = String::from("~/");

        match abs_path.and_then(Self::is_valid_path) {
            Some(abs_path_before_dot) => {
                if let Some((module_before_dot, _)) = module.rsplit_once('.') {
                    source_file_name.push_str(&module_before_dot.replace('.', "/"));
                    source_file_name.push('/');
                }
                source_file_name.push_str(abs_path_before_dot);
            }
            None => {
                let module_before_dollar = module.rsplit_once('$').map(|p| p.0).unwrap_or(module);
                source_file_name.push_str(&module_before_dollar.replace('.', "/"));
            }
        };

        // fake extension because we don't know whether it's .java, .kt or something else
        source_file_name.push_str(".jvm");
        source_file_name
    }
}
