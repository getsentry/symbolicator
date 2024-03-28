use std::collections::HashMap;
use std::sync::OnceLock;

fn java_base_types() -> &'static HashMap<String, String> {
    static JAVA_BASE_TYPES: OnceLock<HashMap<String, String>> = OnceLock::new();
    JAVA_BASE_TYPES.get_or_init(|| {
        // https://docs.oracle.com/javase/8/docs/jdk/api/jpda/jdi/com/sun/jdi/doc-files/signature.html
        HashMap::from([
            ("Z".to_string(), "boolean".to_string()),
            ("B".to_string(), "byte".to_string()),
            ("C".to_string(), "char".to_string()),
            ("S".to_string(), "short".to_string()),
            ("I".to_string(), "int".to_string()),
            ("J".to_string(), "long".to_string()),
            ("F".to_string(), "float".to_string()),
            ("D".to_string(), "double".to_string()),
            ("V".to_string(), "void".to_string()),
        ])
    })
}

fn byte_code_type_to_java_type(
    byte_code_type: String,
    mappers: &[&proguard::ProguardMapper],
) -> String {
    if byte_code_type.is_empty() {
        return "".to_string();
    }
    let mut chrs = byte_code_type.chars();
    let token = chrs.next().unwrap();
    if let Some(ty) = java_base_types().get(&token.to_string()) {
        return ty.clone();
    } else if token == 'L' {
        // invalid signature
        let l = chrs.clone().last();
        if l.is_none() || l.unwrap() != ';' {
            return "".to_string();
        }
        chrs.next_back(); // remove final `;`
        let obfuscated = chrs.collect::<String>().replace('/', ".");
        // if mappers.len() > 0 {}
        for mapper in mappers {
            if let Some(mapped) = mapper.remap_class(&obfuscated) {
                return mapped.to_string();
            }
        }
        return obfuscated;
    } else if token == '[' {
        return format!(
            "{}[]",
            byte_code_type_to_java_type(chrs.collect::<String>(), mappers)
        );
    }
    byte_code_type
}

// parse_obfuscated_bytecode_signature will parse an obfuscated signatures into parameter
// and return types that can be then deobfuscated
fn parse_obfuscated_bytecode_signature(signature: &String) -> Option<(Vec<String>, String)> {
    let mut chrs = signature.chars();

    let token = chrs.next();
    if token.unwrap_or_default() != '(' {
        return None;
    }

    let sig = chrs.collect::<String>();
    let split_sign = sig.rsplitn(2, ')').collect::<Vec<&str>>();
    if split_sign.len() != 2 {
        return None;
    }

    let return_type = split_sign[0];
    let parameter_types = split_sign[1];
    if return_type.is_empty() {
        return None;
    }

    let mut types: Vec<String> = Vec::new();
    let mut tmp_buf: Vec<char> = Vec::new();

    let mut param_chrs = parameter_types.chars();
    while let Some(token) = param_chrs.next() {
        if java_base_types().contains_key(&token.to_string()) {
            if !tmp_buf.is_empty() {
                tmp_buf.push(token);
                types.push(tmp_buf.iter().collect());
                tmp_buf.clear();
            } else {
                types.push(token.to_string());
            }
        } else if token == 'L' {
            tmp_buf.push(token);
            while let Some(c) = param_chrs.next() {
                tmp_buf.push(c);
                if c == ';' {
                    break;
                }
            }
            if tmp_buf.is_empty() || !tmp_buf.ends_with(&[';']) {
                return None;
            }
            types.push(tmp_buf.iter().collect());
            tmp_buf.clear();
        } else if token == '[' {
            tmp_buf.push('[');
        } else {
            tmp_buf.clear();
        }
    }

    Some((types, return_type.to_string()))
}

// returns a tuple where the first element is the list of the function
// parameters and the second one is the return type
pub fn deobfuscate_bytecode_signature(
    signature: &String,
    mappers: &[&proguard::ProguardMapper],
) -> Option<(Vec<String>, String)> {
    if let Some((parameter_types, return_type)) = parse_obfuscated_bytecode_signature(signature) {
        let mut parameter_java_types: Vec<String> = Vec::with_capacity(parameter_types.len());
        for parameter_type in parameter_types {
            let new_class = byte_code_type_to_java_type(parameter_type, mappers);
            parameter_java_types.push(new_class);
        }

        let return_java_type = byte_code_type_to_java_type(return_type, mappers);

        return Some((parameter_java_types, return_java_type));
    }

    None
}

// format_signature formats the types into a human-readable signature
pub fn format_signature(types: &Option<(Vec<String>, String)>) -> String {
    if types.is_none() {
        return "".to_string();
    }

    let (parameter_java_types, return_java_type) = types.as_ref().unwrap();

    let mut signature = format!("({})", parameter_java_types.join(", "));
    if !return_java_type.is_empty() && return_java_type != "void" {
        signature += format!(": {}", return_java_type).as_str();
    }

    signature
}

#[cfg(test)]
mod tests {
    use super::*;
    use proguard::{ProguardMapper, ProguardMapping};
    use std::collections::HashMap;

    #[test]
    fn test_byte_code_type_to_java_type() {
        let proguard_source = b"org.slf4j.helpers.Util$ClassContextSecurityManager -> org.a.b.g$a:
    65:65:void <init>() -> <init>";

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new(mapping);

        let tests = HashMap::from([
            // invalid types
            ("", ""),
            ("L", ""),
            ("", ""),
            // valid types
            ("[I", "int[]"),
            ("I", "int"),
            ("[Ljava/lang/String;", "java.lang.String[]"),
            ("[[J", "long[][]"),
            ("[B", "byte[]"),
            (
                // Obfuscated class type
                "Lorg/a/b/g$a;",
                "org.slf4j.helpers.Util$ClassContextSecurityManager",
            ),
        ]);

        for (ty, expected) in tests {
            assert_eq!(
                byte_code_type_to_java_type(ty.to_string(), &[&mapper]),
                expected.to_string()
            );
        }
    }

    #[test]
    fn test_format_signature() {
        let proguard_source = b"org.slf4j.helpers.Util$ClassContextSecurityManager -> org.a.b.g$a:
    65:65:void <init>() -> <init>";

        let mapping = ProguardMapping::new(proguard_source);
        let mapper = ProguardMapper::new(mapping);

        let tests = HashMap::from([
            // invalid signatures
            ("", ""),
            ("()", ""),
            ("(L)", ""),
            // valid signatures
            ("()V", "()"),
            ("([I)V", "(int[])"),
            ("(III)V", "(int, int, int)"),
            ("([Ljava/lang/String;)V", "(java.lang.String[])"),
            ("([[J)V", "(long[][])"),
            ("(I)I", "(int): int"),
            ("([B)V", "(byte[])"),
            (
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                "(java.lang.String, java.lang.String): java.lang.String",
            ),
            (
                // Obfuscated class type
                "(Lorg/a/b/g$a;)V",
                "(org.slf4j.helpers.Util$ClassContextSecurityManager)",
            ),
        ]);

        for (obfuscated, expected) in tests {
            let signature = deobfuscate_bytecode_signature(&obfuscated.to_string(), &[&mapper]);
            assert_eq!(format_signature(&signature), expected.to_string(),);
        }
    }
}
