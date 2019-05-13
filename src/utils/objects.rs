use symbolic::debuginfo::Object;

use crate::types::ObjectId;

/// Validates that the object matches expected identifiers.
#[allow(unused)]
pub fn validate_object_ids(object: &Object<'_>, identifiers: &ObjectId) -> bool {
    if let Some(ref debug_id) = identifiers.debug_id {
        let parsed_id = object.debug_id();

        // Microsoft symbol server sometimes stores updated files with a more recent
        // (=higher) age, but resolves it for requests with lower ages as well. Thus, we
        // need to check whether the parsed debug file fullfills the *miniumum* age bound.
        // For example:
        // `4A236F6A0B3941D1966B41A4FC77738C2` is reported as
        // `4A236F6A0B3941D1966B41A4FC77738C4` from the server.
        //                                  ^
        if parsed_id.uuid() != debug_id.uuid() || parsed_id.appendix() < debug_id.appendix() {
            metric!(counter("object.debug_id_mismatch") += 1);
            log::debug!(
                "Debug id mismatch. got {}, expected {}",
                object.debug_id(),
                debug_id
            );
            return false;
        }
    }

    if let Some(ref code_id) = identifiers.code_id {
        if let Some(ref object_code_id) = object.code_id() {
            if object_code_id != code_id {
                metric!(counter("object.code_id_mismatch") += 1);
                log::debug!(
                    "Code id mismatch. got {}, expected {}",
                    object_code_id,
                    code_id
                );
                return false;
            }
        }
    }

    true
}
