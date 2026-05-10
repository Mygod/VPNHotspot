use std::io;

use crate::firewall::{self, IptablesTarget};

pub(crate) async fn read_counter_lines() -> io::Result<Vec<String>> {
    Ok(firewall::restore_stdout(
        IptablesTarget::Ipv4,
        "*filter
-nvx -L vpnhotspot_stats
COMMIT
",
    )
    .await?
    .lines()
    .filter(|line| is_counter_line(line))
    .map(str::to_owned)
    .collect())
}

fn is_counter_line(line: &str) -> bool {
    let mut columns = line.split_whitespace();
    columns
        .next()
        .is_some_and(|value| value.parse::<u64>().is_ok())
        && columns
            .next()
            .is_some_and(|value| value.parse::<u64>().is_ok())
}
