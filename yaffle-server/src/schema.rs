use std::{error::Error, net::IpAddr};

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime};
use dns_lookup::lookup_addr;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tantivy::schema::Schema;
use tokio::task;

use crate::gelf::GELFMessage;
use crate::syslog::SyslogMessage;
use yaffle_macros::YaffleSchema;

pub(crate) trait YaffleSchema {
    fn from_gelf(gelf_msg: &GELFMessage) -> Result<Self, Box<dyn Error>>
    where
        Self: Sized;

    fn from_syslog(syslog_msg: &SyslogMessage) -> Result<Self, Box<dyn Error>>
    where
        Self: Sized;

    fn tantivy_schema() -> Schema;

    fn convert_datetime_to_usec(dt: DateTime<FixedOffset>) -> u64 {
        dt.timestamp_nanos() as u64 / 1000u64
    }

    fn convert_float_sec_to_usec(val: &Value) -> Result<u64, Box<dyn Error>> {
        match val {
            Value::Number(n) => n
                .as_f64()
                .map(|v| (v * 1_000_000f64) as u64)
                .ok_or(format!("Can't represent {} as f64", n).into()),
            Value::String(s) => Ok(s.parse().map(|v: f64| (v * 1_000_000f64) as u64)?),
            _ => Err(format!("Can't convert {} to u64", val).into()),
        }
    }

    fn convert_hex_to_uint(val: &Value) -> Result<u64, Box<dyn Error>> {
        match val {
            Value::String(s) => Ok(u64::from_str_radix(&s, 16)?),
            Value::Number(n) => n
                .as_u64()
                .ok_or(format!("Can't represent {} as u64", n).into()),
            _ => Err(format!("Can't convert {} to u64", val).into()),
        }
    }

    fn convert_syslog_timestamp(val: &Value) -> Result<u64, Box<dyn Error>> {
        if let Value::String(ref s) = val {
            Ok(DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.timestamp_nanos() / 1000)
                .or_else(|_e| DateTime::parse_from_rfc2822(s).map(|dt| dt.timestamp_nanos() / 1000))
                .or_else(|_e| {
                    let now_s = format!("{} {}", Local::now().format("%Y"), s);
                    NaiveDateTime::parse_from_str(&now_s, "%Y %b %e %T")
                        .map(|naive| naive.timestamp_nanos() / 1000)
                })? as u64)
        } else {
            Err(format!("Can't parse {} as syslog timestamp", val).into())
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, YaffleSchema)]
pub(crate) struct Document {
    #[from_gelf(timestamp = "float_sec_to_usec", _SOURCE_REALTIME_TIMESTAMP)]
    #[from_syslog(source_timestamp = "datetime_to_usec")]
    #[toshi_type = "timestamp"]
    source_timestamp: Option<u64>,

    #[from_gelf(host, _HOSTNAME)]
    #[from_syslog(hostname)]
    #[toshi_type = "string"]
    hostname: Option<String>,

    #[from_gelf(short_message)]
    #[from_syslog(message)]
    #[toshi_type = "text"]
    message: Option<String>,

    #[from_gelf(level, _PRIORITY)]
    #[from_syslog(priority)]
    #[toshi_type = "u64"]
    #[format = "syslog_priority"]
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<u64>,

    #[from_gelf(full_message)]
    #[from_syslog(full_message)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    full_message: Option<String>,

    #[from_gelf(_MESSAGE_ID)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,

    #[from_gelf(file, _CODE_FILE)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    code_file: Option<String>,

    #[from_gelf(line, _CODE_LINE)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    code_line: Option<u64>,

    #[from_gelf(_CODE_FUNC, _function)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    code_func: Option<String>,

    #[from_gelf(_ERRNO)]
    #[toshi_type = "i64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    errno: Option<i64>,

    #[from_gelf(_INVOCATION_ID)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    invocation_id: Option<String>,

    #[from_gelf(_USER_INVOCATION_ID)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    user_invocation_id: Option<String>,

    #[from_gelf(facility, _facility, _SYSLOG_FACILITY)]
    #[from_syslog(facility)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    facility: Option<String>,

    #[from_gelf(_SYSLOG_IDENTIFIER)]
    #[from_syslog(identifier)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    syslog_identifier: Option<String>,

    #[from_gelf(_SYSLOG_PID)]
    #[from_syslog(pid)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    syslog_pid: Option<u64>,

    #[from_gelf(_SYSLOG_TIMESTAMP = "syslog_timestamp")]
    #[toshi_type = "timestamp"]
    #[serde(skip_serializing_if = "Option::is_none")]
    syslog_timestamp: Option<u64>,

    #[from_gelf(_PID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u64>,

    #[from_gelf(_UID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    uid: Option<u64>,

    #[from_gelf(_GID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    gid: Option<u64>,

    #[from_gelf(_COMM)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    comm: Option<String>,

    #[from_gelf(_EXE)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    exe: Option<String>,

    #[from_gelf(_CMDLINE)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    cmdline: Option<String>,

    #[from_gelf(_CAP_EFFECTIVE = "hex_to_uint")]
    #[toshi_type = "u64"]
    #[format = "hex"]
    #[serde(skip_serializing_if = "Option::is_none")]
    cap_effective: Option<u64>,

    #[from_gelf(_AUDIT_SESSION)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_session: Option<u64>,

    #[from_gelf(_AUDIT_LOGINUID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_loginuid: Option<u64>,

    #[from_gelf(_SYSTEMD_CGROUP)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    systemd_cgroup: Option<String>,

    #[from_gelf(_SYSTEMD_SLICE)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    systemd_slice: Option<String>,

    #[from_gelf(_SYSTEMD_UNIT)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    systemd_unit: Option<String>,

    #[from_gelf(_SYSTEMD_USER_UNIT)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    systemd_user_unit: Option<String>,

    #[from_gelf(_SYSTEMD_USER_SLICE)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    systemd_user_slice: Option<String>,

    #[from_gelf(_SYSTEMD_SESSION)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    systemd_session: Option<String>,

    #[from_gelf(_SYSTEMD_OWNER_UID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    systemd_owner_uid: Option<u64>,

    #[from_gelf(_SELINUX_CONTEXT)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    selinux_context: Option<String>,

    #[from_gelf(_BOOT_ID)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    boot_id: Option<String>,

    #[from_gelf(_MACHINE_ID)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    machine_id: Option<String>,

    #[from_gelf(_SYSTEMD_INVOCATION_ID)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    systemd_invocation_id: Option<String>,

    #[from_gelf(_TRANSPORT)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,

    #[from_gelf(_STREAM_ID)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_id: Option<String>,

    #[from_gelf(_LINE_BREAK)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    line_break: Option<String>,

    #[from_gelf(_NAMESPACE)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,

    #[from_gelf(_KERNEL_DEVICE)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_device: Option<String>,

    #[from_gelf(_KERNEL_SUBSYSTEM)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_subsystem: Option<String>,

    #[from_gelf(_UDEV_SYSNAME)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    udev_sysname: Option<String>,

    #[from_gelf(_UDEV_DEVNODE)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    udev_devnode: Option<String>,

    #[from_gelf(_UDEV_DEVLINK)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    udev_devlink: Option<String>,

    #[from_gelf(_COREDUMP_UNIT)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    coredump_unit: Option<String>,

    #[from_gelf(_COREDUMP_USER_UNIT)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    coredump_user_unit: Option<String>,

    #[from_gelf(_OBJECT_PID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_pid: Option<u64>,

    #[from_gelf(_OBJECT_UID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_uid: Option<u64>,

    #[from_gelf(_OBJECT_GID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_gid: Option<u64>,

    #[from_gelf(_OBJECT_COMM)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_comm: Option<String>,

    #[from_gelf(_OBJECT_EXE)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_exe: Option<String>,

    #[from_gelf(_OBJECT_CMDLINE)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_cmdline: Option<String>,

    #[from_gelf(_OBJECT_AUDIT_SESSION)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_audit_session: Option<u64>,

    #[from_gelf(_OBJECT_AUDIT_LOGINUID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_audit_loginuid: Option<u64>,

    #[from_gelf(_OBJECT_SYSTEMD_CGROUP)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_systemd_cgroup: Option<String>,

    #[from_gelf(_OBJECT_SYSTEMD_SESSION)]
    #[toshi_type = "string"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_systemd_session: Option<String>,

    #[from_gelf(_OBJECT_SYSTEMD_OWNER_UID)]
    #[toshi_type = "u64"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_systemd_owner_uid: Option<u64>,

    #[from_gelf(_OBJECT_SYSTEMD_UNIT)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_systemd_unit: Option<String>,

    #[from_gelf(_OBJECT_SYSTEMD_USER_UNIT)]
    #[toshi_type = "text"]
    #[serde(skip_serializing_if = "Option::is_none")]
    object_systemd_user_unit: Option<String>,

    #[from_gelf(___REALTIME_TIMESTAMP)]
    #[toshi_type = "timestamp"]
    #[serde(skip_serializing_if = "Option::is_none")]
    recv_rt_timestamp: Option<u64>,

    #[from_gelf(___MONOTONIC_TIMESTAMP)]
    #[toshi_type = "timestamp"]
    #[serde(skip_serializing_if = "Option::is_none")]
    recv_mt_timestamp: Option<u64>,
}

impl Document {
    pub async fn do_reverse_dns(&mut self, src_ip: IpAddr) {
        if self.hostname.is_none() {
            self.hostname.replace(
                task::spawn_blocking(move || lookup_addr(&src_ip))
                    .await
                    .unwrap_or(Ok(src_ip.to_string()))
                    .unwrap_or(src_ip.to_string())
                    .into(),
            );
        }
    }

    pub fn is_valid(&self) -> bool {
        self.source_timestamp.map(|ts| ts > 0).unwrap_or(false)
            && self
                .message
                .as_ref()
                .map(|msg| msg.len() > 0)
                .unwrap_or(false)
    }
}
