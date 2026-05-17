use std::io;

use crate::{
    firewall::{self, IptablesTarget},
    report,
};
use vpnhotspotd::shared::protocol::IoResultReportExt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IptablesRule {
    pub(super) target: IptablesTarget,
    pub(super) table: &'static str,
    pub(super) chain: &'static str,
    args: Vec<String>,
}

impl IptablesRule {
    pub(super) fn new(
        target: IptablesTarget,
        table: &'static str,
        chain: &'static str,
        args: Vec<String>,
    ) -> Self {
        Self {
            target,
            table,
            chain,
            args,
        }
    }

    fn insert_line(&self) -> io::Result<String> {
        firewall::restore_line("-I", self.chain, &self.args)
    }

    pub(super) async fn delete_repeated(&self) {
        let input = match self.delete_input() {
            Ok(input) => input,
            Err(e) => {
                report::io_with_details("routing.iptables_delete_repeated", e, self.details());
                return;
            }
        };
        loop {
            match firewall::restore_status(self.target, &input).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    report::io_with_details(
                        "routing.iptables_delete_repeated",
                        e,
                        firewall::restore_details(self.target, &input),
                    );
                    break;
                }
            }
        }
    }

    pub(super) async fn delete(&self) -> bool {
        match self.delete_input() {
            Ok(input) => {
                if let Err(e) = firewall::restore_status(self.target, &input).await {
                    report::io_with_details("routing.iptables_delete", e, self.details());
                    false
                } else {
                    true
                }
            }
            Err(e) => {
                report::io_with_details("routing.iptables_delete", e, self.details());
                false
            }
        }
    }

    fn delete_input(&self) -> io::Result<String> {
        Ok(firewall::restore_input(
            self.table,
            &[firewall::restore_line("-D", self.chain, &self.args)?],
        ))
    }

    pub(super) fn details(&self) -> Vec<(String, String)> {
        vec![
            ("binary".to_owned(), self.target.restore_binary().to_owned()),
            ("table".to_owned(), self.table.to_owned()),
            ("chain".to_owned(), self.chain.to_owned()),
            ("args".to_owned(), self.args.join(" ")),
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct IptablesChain {
    pub(super) target: IptablesTarget,
    pub(super) table: &'static str,
    pub(super) chain: &'static str,
}

impl IptablesChain {
    pub(super) const fn new(
        target: IptablesTarget,
        table: &'static str,
        chain: &'static str,
    ) -> Self {
        Self {
            target,
            table,
            chain,
        }
    }
}

pub(super) async fn apply_iptables_batch(
    target: IptablesTarget,
    table: &str,
    rules: &[IptablesRule],
) -> io::Result<()> {
    let mut lines = Vec::with_capacity(rules.len());
    for rule in rules {
        lines.push(
            rule.insert_line()
                .with_report_context_details("routing.iptables_insert.line", rule.details())?,
        );
    }
    firewall::restore(target, &firewall::restore_input(table, &lines)).await
}

pub(super) async fn ensure_iptables_chain(target: IptablesTarget, table: &str, chain: &str) {
    match firewall::restore_line("-N", chain, &[]) {
        Ok(line) => {
            let input = firewall::restore_input(table, &[line]);
            if let Err(e) = firewall::restore_status(target, &input).await {
                report::io_with_details(
                    "routing.iptables_new_chain",
                    e,
                    firewall::restore_details(target, &input),
                );
            }
        }
        Err(e) => report::io_with_details(
            "routing.iptables_new_chain",
            e,
            [
                ("binary", target.restore_binary().to_owned()),
                ("table", table.to_owned()),
                ("chain", chain.to_owned()),
            ],
        ),
    }
}

pub(super) async fn ensure_iptables_chain_result(
    target: IptablesTarget,
    table: &str,
    chain: &str,
) -> io::Result<()> {
    let input = firewall::restore_input(table, &[firewall::restore_line("-N", chain, &[])?]);
    firewall::restore_status(target, &input).await?;
    Ok(())
}

pub(super) async fn delete_iptables_repeated(
    target: IptablesTarget,
    table: &'static str,
    chain: &'static str,
    args: &[&str],
) {
    IptablesRule::new(
        target,
        table,
        chain,
        args.iter().map(|arg| (*arg).to_owned()).collect(),
    )
    .delete_repeated()
    .await;
}
