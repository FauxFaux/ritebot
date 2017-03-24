#[macro_use] extern crate diesel;
#[macro_use] extern crate diesel_codegen;
extern crate dotenv;
extern crate irc;

pub mod schema;
pub mod models;

use std::env;
use std::io;
use std::thread;
use std::time;

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use irc::client::prelude::*;

struct Bot {
    earliest_wake: u64,
}

fn other(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}

fn establish_connection() -> SqliteConnection {
    dotenv::dotenv().ok();
    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set");
    SqliteConnection::establish(&database_url)
        .expect(&format!("Error connecting to {}", database_url))
}

fn parse_period(period: &str) -> Result<u64, String> {
    let mut ms = 0u64;
    let mut remain = period;
    loop {
        let num_end = match remain.find(|x: char| -> bool { !x.is_digit(10) }) {
            Some(pos) => pos,
            None => break,
        };

        if 0 == num_end {
            return Err(format!("expecting a number at: '{}'", remain));
        }

        let num: u64 = remain[..num_end].parse().map_err(|e| format!("invalid number: {}", e))?;
        remain = &remain[num_end..];

        let text = match remain.find(|x: char| -> bool { x.is_digit(10) }) {
            Some(pos) => {
                if 0 == pos {
                    return Err(format!("expecting a text at: '{}'", remain));
                }

                let text = &remain[..pos];
                remain = &remain[pos..];
                text
            },
            None => {
                let text = remain;
                remain = "";
                text
            }
        };

        ms += num * match text {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60 * 1_000,
            "h" => 60 * 60 * 1_000,
            "d" => 24 * 60 * 60 * 1_000,
            "w" => 7 * 24 * 60 * 60 * 1_000,
            "mo" => (365.24 / 12. * 60. * 60. * 1_000.) as u64,
            _ => return Err(format!("unsupported duration code: {}", text)),
        };
    };

    if !remain.is_empty() {
        return Err(format!("trailing unparsable junk: '{}'", remain));
    }

    Ok(ms)
}

#[test]
fn period() {
    assert_eq!(parse_period("1m"), Ok(60_000));
    assert_eq!(parse_period("1m23s456ms1m"), Ok((2 * 60 + 23) * 1_000 + 456));
}

fn now_ms() -> u64 {
    let dur = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    return (dur.as_secs() * 1_000) + (dur.subsec_nanos() as u64 / 1_000_000);
}

fn command_in(conn: &SqliteConnection, bot: &mut Bot, whom: &str, arg: &str) -> io::Result<String> {
    let mut args = arg.splitn(3, ' ');
    let period = match args.next() {
        Some(val) => val,
        None => return Ok("syntax error: in requires a period".to_string()),
    };
    let subcmd = match args.next() {
        Some(val) => val,
        None => return Ok("syntax error: in requires a sub command".to_string()),
    };
    let text = match args.next() {
        Some(val) => val,
        None => return Ok("syntax error: in requires text".to_string()),
    };

    let duration = match parse_period(period) {
        Ok(dur) => dur,
        Err(msg) => return Ok(format!("invalid period '{}': {}", period, msg)),
    };

    if subcmd != "reply" {
        return Ok(format!("subcommand must be 'reply', not '{}'", subcmd));
    }

    let when_ms = now_ms() + duration;
    if when_ms > std::i64::MAX as u64 {
        return Ok("that's too far in the future!".to_string());
    }

    let new_timer = models::NewTimer {
        at: when_ms as i64,
        whom,
        operation: text,
    };

    if when_ms < bot.earliest_wake {
        bot.earliest_wake = when_ms;
    }

    diesel::insert(&new_timer).into(schema::timers::table)
        .execute(conn).unwrap();

    Ok(format!("Will reply '{}' at '{}'", text, when_ms))
}

fn process<S: ServerExt>(server: &S,
                         conn: &SqliteConnection,
                         bot: &mut Bot,
                         message: &Message) -> io::Result<()> {
    match message.command {
        Command::PRIVMSG(ref target, ref msg) => {
            let src = message.source_nickname().ok_or(other("no source nick on privmsg?"))?;
            let is_channel = target.starts_with('#');
            let tagged_command = msg.starts_with('¡');
            if !tagged_command && is_channel {
                return Ok(());
            }
            let command_line = if tagged_command {
                &msg[2..]
            } else {
                msg
            };
            let command = match command_line.find(' ') {
                Some(e) => (&command_line[..e], &command_line[(e+1)..]),
                None => (command_line, ""),
            };

            let response = match command {
                ("in", e) => command_in(conn, bot, src, e)?,
                (e, _) => format!("unknown command: {}", e),
            };

            if response.is_empty() {
                return Ok(());
            }

            if is_channel {
                server.send_notice(target, format!("{}: {}", src, response).as_str())
            } else {
                server.send_privmsg(src, response.as_str())
            }
        },
        _ => Ok(()),
    }
}

fn load_earliest(bot: &mut Bot, conn: &SqliteConnection) -> Result<(), String> {
    use schema::timers::dsl::*;
    use diesel::expression::dsl::min;

    let next = timers.select(min(at)).get_result::<Option<i64>>(conn)
        .map_err(|e| format!("next: {}", e))?;
    bot.earliest_wake = next.unwrap_or(std::i64::MAX) as u64;
    Ok(())
}

fn worker<S: ServerExt>(bot: &mut Bot, server: &S, conn: &SqliteConnection) -> Result<(), String> {
    let now = now_ms();
    if bot.earliest_wake > now {
        return Ok(());
    }

    use schema::timers::dsl::*;
    let passed = timers.filter(at.le(now as i64)).load::<models::Timer>(conn)
        .map_err(|e| format!("select: {}", e))?;

    if passed.is_empty() {
        return Ok(());
    }

    for timer in passed {
        server.send_notice(timer.whom.as_str(), timer.operation.as_str())
            .map_err(|e| format!("send: {}", e))?;
    }

    diesel::delete(timers.filter(at.le(now as i64))).execute(conn)
        .map_err(|e| format!("delete: {}", e))?;
    load_earliest(bot, conn)?;
    Ok(())
}

fn main() {
    let conn = establish_connection();
    let arc = Arc::new(Mutex::new(Bot { earliest_wake: std::u64::MAX }));
    let server = IrcServer::new("config.json").unwrap();
    server.identify().unwrap();
    {
        let server = server.clone();
        let arc = arc.clone();
        thread::spawn(move || {
            let conn = establish_connection();
            loop {
                thread::sleep(time::Duration::from_millis(2_000));
                let mut bot = arc.lock().unwrap();
                if let Err(e) = worker(&mut *bot, &server, &conn) {
                    println!("thread problem: {}", e);
                }
            }
        });
    }
    load_earliest(&mut *arc.lock().unwrap(), &conn).unwrap();
    for message_result in server.iter() {
        let message = message_result.expect("valid message");
        let mut bot = arc.lock().unwrap();
        if let Err(e) = process(&server, &conn, &mut bot, &message) {
            println!("failed processing {}: {}", message, e);
        }
    }
}
