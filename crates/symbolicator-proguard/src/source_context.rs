use symbolicator_service::source_context::get_context_lines;

use crate::{interface::JvmFrame, ProguardService};

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

    match abs_path.and_then(is_valid_path) {
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

fn apply_source_context(frame: &mut JvmFrame, source: &str) {
    let lineno = frame.lineno as usize;

    if let Some((pre_context, context_line, post_context)) =
        get_context_lines(source, lineno, None, None)
    {
        frame.pre_context = pre_context;
        frame.context_line = Some(context_line);
        frame.post_context = post_context;
    }
}
