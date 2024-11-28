use std::fs::File;
use std::sync::Arc;

use anyhow::{Context, Result};
use apple_crash_report_parser::AppleCrashReport;
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use symbolic::common::{Arch, CodeId, DebugId};
use symbolicator_service::types::{Platform, RawObjectInfo, Scope, ScrapingConfig};
use symbolicator_service::utils::hex::HexValue;
use symbolicator_sources::{ObjectType, SourceConfig};

use crate::interface::{
    CompleteObjectInfo, CompletedSymbolicationResponse, RawFrame, RawStacktrace,
    SymbolicateStacktraces, SystemInfo,
};
use crate::metrics::StacktraceOrigin;

use super::symbolicate::SymbolicationActor;

impl SymbolicationActor {
    #[tracing::instrument(skip_all)]
    fn parse_apple_crash_report(
        &self,
        platform: Option<Platform>,
        scope: Scope,
        report: File,
        sources: Arc<[SourceConfig]>,
        scraping: ScrapingConfig,
    ) -> Result<(SymbolicateStacktraces, AppleCrashReportState)> {
        let report =
            AppleCrashReport::from_reader(report).context("failed to parse apple crash report")?;
        let mut metadata = report.metadata;

        let arch = report
            .code_type
            .as_ref()
            .and_then(|code_type| code_type.split(' ').next())
            .and_then(|word| word.parse().ok())
            .unwrap_or_default();

        let modules = report
            .binary_images
            .into_iter()
            .map(map_apple_binary_image)
            .collect();

        let mut stacktraces = Vec::with_capacity(report.threads.len());

        for thread in report.threads {
            let registers = thread
                .registers
                .unwrap_or_default()
                .into_iter()
                .map(|(name, addr)| (name, HexValue(addr.0)))
                .collect();

            let frames = thread
                .frames
                .into_iter()
                .map(|frame| RawFrame {
                    instruction_addr: HexValue(frame.instruction_addr.0),
                    package: frame.module,
                    ..RawFrame::default()
                })
                .collect();

            stacktraces.push(RawStacktrace {
                thread_id: Some(thread.id),
                thread_name: thread.name,
                is_requesting: Some(thread.crashed),
                registers,
                frames,
            });
        }

        let request = SymbolicateStacktraces {
            platform,
            modules,
            scope,
            sources,
            origin: StacktraceOrigin::AppleCrashReport,
            signal: None,
            stacktraces,
            apply_source_context: true,
            scraping,
        };

        let mut system_info = SystemInfo {
            os_name: metadata.remove("OS Version").unwrap_or_default(),
            device_model: metadata.remove("Hardware Model").unwrap_or_default(),
            cpu_arch: arch,
            ..SystemInfo::default()
        };

        if let Some(captures) = OS_MACOS_REGEX.captures(&system_info.os_name) {
            system_info.os_version = captures
                .name("version")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            system_info.os_build = captures
                .name("build")
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            system_info.os_name = "macOS".to_string();
        }

        // https://developer.apple.com/library/archive/technotes/tn2151/_index.html
        let crash_reason = metadata.remove("Exception Type");
        let crash_details = report
            .application_specific_information
            .or_else(|| metadata.remove("Exception Message"))
            .or_else(|| metadata.remove("Exception Subtype"))
            .or_else(|| metadata.remove("Exception Codes"));

        let state = AppleCrashReportState {
            timestamp: report.timestamp,
            system_info,
            crash_reason,
            crash_details,
        };

        Ok((request, state))
    }

    pub async fn process_apple_crash_report(
        &self,
        platform: Option<Platform>,
        scope: Scope,
        report: File,
        sources: Arc<[SourceConfig]>,
        scraping: ScrapingConfig,
    ) -> Result<CompletedSymbolicationResponse> {
        let (request, state) =
            self.parse_apple_crash_report(platform, scope, report, sources, scraping)?;
        let mut response = self.symbolicate(request).await?;

        state.merge_into(&mut response);
        Ok(response)
    }
}

/// Format sent by Unreal Engine on macOS
static OS_MACOS_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^Mac OS X (?P<version>\d+\.\d+\.\d+)( \((?P<build>[a-fA-F0-9]+)\))?$").unwrap()
});

#[derive(Debug)]
struct AppleCrashReportState {
    timestamp: Option<DateTime<Utc>>,
    system_info: SystemInfo,
    crash_reason: Option<String>,
    crash_details: Option<String>,
}

impl AppleCrashReportState {
    fn merge_into(mut self, response: &mut CompletedSymbolicationResponse) {
        if self.system_info.cpu_arch == Arch::Unknown {
            self.system_info.cpu_arch = response
                .modules
                .iter()
                .map(|object| object.arch)
                .find(|arch| *arch != Arch::Unknown)
                .unwrap_or_default();
        }

        response.timestamp = self.timestamp;
        response.system_info = Some(self.system_info);
        response.crash_reason = self.crash_reason;
        response.crash_details = self.crash_details;
        response.crashed = Some(true);
    }
}

fn map_apple_binary_image(image: apple_crash_report_parser::BinaryImage) -> CompleteObjectInfo {
    let code_id = CodeId::from_binary(&image.uuid.as_bytes()[..]);
    let debug_id = DebugId::from_uuid(image.uuid);

    let raw_info = RawObjectInfo {
        ty: ObjectType::Macho,
        code_id: Some(code_id.to_string()),
        code_file: Some(image.path.clone()),
        debug_id: Some(debug_id.to_string()),
        debug_file: Some(image.path),
        debug_checksum: None,
        image_addr: HexValue(image.addr.0),
        image_size: match image.size {
            0 => None,
            size => Some(size),
        },
    };

    raw_info.into()
}
