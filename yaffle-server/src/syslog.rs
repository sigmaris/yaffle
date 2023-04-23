use std::error::Error;
use std::net::SocketAddr;

use chrono::{
    DateTime, Datelike, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone,
};
use log::warn;
use nom::branch::alt;
use nom::bytes::complete::{tag, take_until, take_while1, take_while_m_n};
use nom::character::complete::{char, digit1};
use nom::character::{is_alphanumeric, is_digit, is_space};
use nom::combinator::{map, map_res, opt, recognize, rest, value};
use nom::sequence::{delimited, preceded, separated_pair, terminated, tuple};
use nom::IResult;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::Sender;

#[derive(Debug)]
pub struct SyslogMessage {
    pub(crate) priority: u8,
    pub(crate) facility: String,
    pub(crate) source_timestamp: DateTime<FixedOffset>,
    pub(crate) hostname: Option<String>,
    pub(crate) identifier: Option<String>,
    pub(crate) pid: Option<u64>,
    pub(crate) message: String,
    pub(crate) full_message: String,
}

const FACILITY_NAMES: [&str; 16] = [
    "kern",     // #define LOG_KERN        (0<<3)  /* kernel messages */
    "user",     // #define LOG_USER        (1<<3)  /* random user-level messages */
    "mail",     // #define LOG_MAIL        (2<<3)  /* mail system */
    "daemon",   // #define LOG_DAEMON      (3<<3)  /* system daemons */
    "auth",     // #define LOG_AUTH        (4<<3)  /* security/authorization messages */
    "syslog",   // #define LOG_SYSLOG      (5<<3)  /* messages generated internally by syslogd */
    "lpr",      // #define LOG_LPR         (6<<3)  /* line printer subsystem */
    "news",     // #define LOG_NEWS        (7<<3)  /* network news subsystem */
    "uucp",     // #define LOG_UUCP        (8<<3)  /* UUCP subsystem */
    "cron",     // #define LOG_CRON        (9<<3)  /* clock daemon */
    "authpriv", // #define LOG_AUTHPRIV    (10<<3) /* security/authorization messages (private) */
    "ftp",      // #define LOG_FTP         (11<<3) /* ftp daemon */
    "ntp", "audit", "alert", "clock", // Other facilities from RFC3164
];

fn prio_and_facility(input: &[u8]) -> IResult<&[u8], (u8, String)> {
    map(
        map_res(
            map_res(delimited(char('<'), digit1, char('>')), std::str::from_utf8),
            |digits| u8::from_str_radix(digits, 10),
        ),
        |combined| {
            let facility = combined >> 3;
            let priority = combined & 0x7;
            let facility_name = if facility < 16 {
                FACILITY_NAMES[facility as usize].to_string()
            } else {
                format!("local{}", facility - 16)
            };
            (priority, facility_name)
        },
    )(input)
}

fn rfc3339_timestamp(input: &[u8]) -> IResult<&[u8], DateTime<FixedOffset>> {
    map_res(map_res(take_until(" "), std::str::from_utf8), |ts_str| {
        DateTime::parse_from_rfc3339(ts_str)
    })(input)
}

fn rfc3164_timestamp(input: &[u8]) -> IResult<&[u8], DateTime<FixedOffset>> {
    let month = alt((
        value(1u8, tag("Jan ")),
        value(2u8, tag("Feb ")),
        value(3u8, tag("Mar ")),
        value(4u8, tag("Apr ")),
        value(5u8, tag("May ")),
        value(6u8, tag("Jun ")),
        value(7u8, tag("Jul ")),
        value(8u8, tag("Aug ")),
        value(9u8, tag("Sep ")),
        value(10u8, tag("Oct ")),
        value(11u8, tag("Nov ")),
        value(12u8, tag("Dec ")),
    ));
    let day = map_res(
        map_res(
            terminated(
                alt((
                    preceded(tag(" "), take_while_m_n(1, 1, is_digit)),
                    take_while_m_n(2, 2, is_digit),
                )),
                tag(" "),
            ),
            std::str::from_utf8,
        ),
        |s| u8::from_str_radix(s, 10),
    );
    let time = map_res(
        map_res(
            terminated(
                recognize(separated_pair(
                    separated_pair(
                        take_while_m_n(2, 2, is_digit),
                        tag(":"),
                        take_while_m_n(2, 2, is_digit),
                    ),
                    tag(":"),
                    take_while_m_n(2, 2, is_digit),
                )),
                tag(" "),
            ),
            std::str::from_utf8,
        ),
        |s| NaiveTime::parse_from_str(s, "%H:%M:%S"),
    );
    let ts_tuple = tuple((month, day, time));
    let naive_ts = map_res(ts_tuple, |(mon, d, naive_time)| {
        NaiveDate::from_ymd_opt(Local::now().year(), mon.into(), d.into())
            .map(|naive_date| NaiveDateTime::new(naive_date, naive_time))
            .ok_or(format!(
                "Invalid date: year={}, month={}, day={}",
                Local::now().year(),
                mon,
                d
            ))
    });
    map_res(naive_ts, |naive| {
        match Local.from_local_datetime(&naive) {
            LocalResult::None => Err(format!("Invalid timestamp: {}", naive)),
            LocalResult::Single(ts) => Ok(ts),
            LocalResult::Ambiguous(ts1, ts2) => {
                warn!(
                    "Ambiguous timestamps: {} and {}, choosing {}",
                    ts1, ts2, ts1
                );
                Ok(ts1)
            }
        }
        .map(|dt| dt.with_timezone(dt.offset()))
    })(input)
}

fn hostname(input: &[u8]) -> IResult<&[u8], &[u8]> {
    terminated(
        take_while1(|char| is_alphanumeric(char) || char == b'.'),
        tag(" "),
    )(input)
}

fn identifier_and_pid(input: &[u8]) -> IResult<&[u8], (&[u8], Option<u64>)> {
    terminated(
        tuple((
            take_while1(|ch| ch != b':' && ch != b'[' && !is_space(ch)),
            opt(map_res(
                map_res(
                    preceded(tag("["), terminated(digit1, tag("]"))),
                    std::str::from_utf8,
                ),
                |s| u64::from_str_radix(s, 10),
            )),
        )),
        tag(": "),
    )(input)
}

fn parse_syslog(input: &[u8]) -> IResult<&[u8], SyslogMessage> {
    map(
        tuple((
            opt(prio_and_facility),
            opt(alt((
                // RFC 5424 version, space, RFC3339 timestamp
                preceded(terminated(digit1, tag(" ")), rfc3339_timestamp),
                // RFC 3164 timestamp
                rfc3164_timestamp,
            ))),
            opt(hostname),
            opt(identifier_and_pid),
            rest,
        )),
        |(opt_prio_fac, opt_ts, opt_hostname, opt_ident_and_pid, msg)| SyslogMessage {
            priority: *opt_prio_fac.as_ref().map(|(prio, _)| prio).unwrap_or(&5u8),
            facility: opt_prio_fac
                .map(|(_, fac)| fac)
                .unwrap_or_else(|| "user".to_string()),
            source_timestamp: opt_ts.unwrap_or_else(|| {
                let now = Local::now();
                now.with_timezone(now.offset())
            }),
            hostname: opt_hostname.map(|b| String::from_utf8_lossy(b).to_string()),
            identifier: opt_ident_and_pid
                .map(|(ident, _)| String::from_utf8_lossy(ident).to_string()),
            pid: opt_ident_and_pid.map(|(_, pid)| pid).flatten(),
            message: {
                let string_msg = String::from_utf8_lossy(msg);
                if opt_ts.is_none() && opt_hostname.is_none() && opt_ident_and_pid.is_none() {
                    string_msg
                        .strip_prefix(' ')
                        .unwrap_or(&string_msg)
                        .to_string()
                } else {
                    string_msg.to_string()
                }
            },
            full_message: String::from_utf8_lossy(input).to_string(),
        },
    )(input)
}

pub async fn run_recv_loop(
    socket: UdpSocket,
    syslog_pipe: Sender<(SocketAddr, SyslogMessage)>,
) -> Result<(), Box<dyn Error>> {
    let mut buf = [0; 65536];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((size, source)) => match parse_syslog(&buf[0..size]) {
                Ok((remaining, syslog_msg)) => {
                    syslog_pipe.send((source, syslog_msg)).await?;
                    if !remaining.is_empty() {
                        warn!(
                            "Remaining data after parsing syslog message: {:?}",
                            remaining
                        );
                    }
                }
                Err(e) => warn!("Syslog parse error: {}", e),
            },
            Err(e) => warn!("Packet receive error: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::parse_syslog;
    use super::prio_and_facility;
    use super::rfc3164_timestamp;
    use chrono::{Datelike, Local, Timelike};

    #[test]
    fn priority_test() {
        let result = prio_and_facility(b"<165>");
        assert!(result.is_ok());
        let (remaining, (prio, facility)) = result.unwrap();
        assert_eq!(remaining, b"");
        assert_eq!(prio, 5u8);
        assert_eq!(facility, "local4".to_string());
    }

    #[test]
    fn test_rfc3164_timestamp() {
        let result = rfc3164_timestamp(b"Jan  2 12:24:59 ");
        assert!(result.is_ok());
        let (remaining, ts) = result.unwrap();
        assert_eq!(remaining, b"");
        assert_eq!(ts.year(), Local::now().year());
        assert_eq!(ts.month(), 1);
        assert_eq!(ts.day(), 2);
        assert_eq!(ts.hour(), 12);
        assert_eq!(ts.minute(), 24);
        assert_eq!(ts.second(), 59);
    }

    #[test]
    fn test_full_msg_with_timestamp() {
        let result = parse_syslog(b"<78>Aug  2 09:00:00 crond[926]: USER root pid 14786 cmd logger -p syslog.info -- -- MARK --");
        assert!(result.is_ok());
        let (remaining, msg) = result.unwrap();
        assert_eq!(remaining, b"");
        assert_eq!(msg.priority, 6);
        assert_eq!(msg.facility, "cron");
        assert_eq!(msg.source_timestamp.year(), Local::now().year());
        assert_eq!(msg.source_timestamp.month(), 8);
        assert_eq!(msg.source_timestamp.day(), 2);
        assert_eq!(msg.source_timestamp.hour(), 9);
        assert_eq!(msg.source_timestamp.minute(), 0);
        assert_eq!(msg.source_timestamp.second(), 0);
        assert_eq!(msg.identifier, Some("crond".to_string()));
        assert_eq!(msg.pid, Some(926));
        assert_eq!(
            msg.message,
            "USER root pid 14786 cmd logger -p syslog.info -- -- MARK --".to_string()
        )
    }

    #[test]
    fn test_full_message_ident_no_pid() {
        let result = parse_syslog(b"<46>Aug  1 19:00:00 root: -- MARK --");
        assert!(result.is_ok());
        let (remaining, msg) = result.unwrap();
        assert_eq!(remaining, b"");
        assert_eq!(msg.priority, 6);
        assert_eq!(msg.facility, "syslog");
        assert_eq!(msg.source_timestamp.year(), Local::now().year());
        assert_eq!(msg.source_timestamp.month(), 8);
        assert_eq!(msg.source_timestamp.day(), 1);
        assert_eq!(msg.source_timestamp.hour(), 19);
        assert_eq!(msg.source_timestamp.minute(), 0);
        assert_eq!(msg.source_timestamp.second(), 0);
        assert_eq!(msg.hostname, None);
        assert_eq!(msg.identifier, Some("root".to_string()));
        assert_eq!(msg.pid, None);
        assert_eq!(msg.message, "-- MARK --".to_string())
    }

    #[test]
    fn test_full_msg_no_timestamp() {
        let result = parse_syslog(b"<7> [0]DAA FXO: ON-HOOK, PARA HANDSET: OFF-HOOK");
        assert!(result.is_ok());
        let (remaining, msg) = result.unwrap();
        assert_eq!(remaining, b"");
        assert_eq!(msg.priority, 7);
        assert_eq!(msg.facility, "kern");
        assert_eq!(msg.source_timestamp.year(), Local::now().year());
        assert_eq!(msg.source_timestamp.month(), Local::now().month());
        assert_eq!(msg.source_timestamp.day(), Local::now().day());
        assert_eq!(msg.hostname, None);
        assert_eq!(msg.identifier, None);
        assert_eq!(msg.pid, None);
        assert_eq!(
            msg.message,
            "[0]DAA FXO: ON-HOOK, PARA HANDSET: OFF-HOOK".to_string()
        )
    }

    #[test]
    fn test_bare_message() {
        let result = parse_syslog(b"<7> register callback");
        assert!(result.is_ok());
        let (remaining, msg) = result.unwrap();
        assert_eq!(remaining, b"");
        assert_eq!(msg.priority, 7);
        assert_eq!(msg.facility, "kern");
        assert_eq!(msg.source_timestamp.year(), Local::now().year());
        assert_eq!(msg.source_timestamp.month(), Local::now().month());
        assert_eq!(msg.source_timestamp.day(), Local::now().day());
        assert_eq!(msg.hostname, None);
        assert_eq!(msg.identifier, None);
        assert_eq!(msg.pid, None);
        assert_eq!(msg.message, "register callback".to_string())
    }
}
