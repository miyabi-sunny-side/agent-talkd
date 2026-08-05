//! `SO_PEERCRED` の peer PID から、呼び出し元の pane identity を daemon 側で
//! 確立する。
//!
//! herdr は pane 内で起動した agent の process に `HERDR_PANE_ID` /
//! `HERDR_SOCKET_PATH` を与えている。agent が MCP server を env clear で
//! spawn しても、/proc の祖先を遡れば agent 本体の environ にこの 2 key が
//! 残っている。daemon はそこから **必要な 2 key だけ** を読む — environ の
//! 全文を保持・記録しない。cwd やコマンド名からの推測はしない (同種 agent が
//! 同じ directory に 2 つ居ると誤配するため)。曖昧・観測不能は fail closed。

use std::path::Path;

/// 祖先を遡る上限。mcp → (launcher) → agent → shell → herdr 程度で足りる。
/// 上限は循環した親子関係 (PID 再利用の race) で無限に歩かないための保険。
const MAX_ANCESTORS: usize = 16;

/// 祖先の environ から読み取る、herdr が与えた identity。
///
/// **2 key が揃っている祖先だけ**が identity になる。pane id は herdr session
/// 間で衝突しうるため、どの herdr の id かを socket で証明できない片欠けは
/// identity として扱わない (探索を続け、見つからなければ拒否)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrIdentity {
    pub pane: String,
    pub herdr_socket: String,
}

/// peer PID の祖先から pane identity を解決する (Linux)。
///
/// `expected_herdr_socket` はこの daemon が繋いでいる herdr。祖先が別の herdr
/// session に属していれば、その pane id はこの daemon の registry では
/// 別人を指しうるため拒否する。
#[cfg(target_os = "linux")]
pub fn resolve_from_peer(pid: i32, expected_herdr_socket: &Path) -> Result<String, String> {
    resolve_with(pid, expected_herdr_socket, parent_of, herdr_identity_of)
}

#[cfg(not(target_os = "linux"))]
pub fn resolve_from_peer(_pid: i32, _expected_herdr_socket: &Path) -> Result<String, String> {
    Err(
        "この platform では呼び出し元 process からの pane 解決に対応していません \
         (HERDR_SOCKET_PATH と HERDR_PANE_ID を MCP server へ forward してください)"
            .to_owned(),
    )
}

/// 解決の本体。/proc の読み出しを注入可能にしてテストする。
fn resolve_with(
    pid: i32,
    expected_herdr_socket: &Path,
    parent_of: impl Fn(i32) -> Option<i32>,
    identity_of: impl Fn(i32) -> Option<HerdrIdentity>,
) -> Result<String, String> {
    let mut current = pid;
    for _ in 0..MAX_ANCESTORS {
        if let Some(identity) = identity_of(current) {
            // 祖先が別の herdr session に属するなら、この daemon の registry で
            // その pane id を引いても別人を指しうる。黙って bind しない。
            if Path::new(&identity.herdr_socket) != expected_herdr_socket {
                return Err(format!(
                    "呼び出し元は別の herdr ({}) に属しています",
                    identity.herdr_socket
                ));
            }
            return Ok(identity.pane);
        }
        let Some(parent) = parent_of(current) else {
            break;
        };
        if parent <= 1 || parent == current {
            break;
        }
        current = parent;
    }
    Err("呼び出し元 process の祖先に herdr の pane identity が見つかりません".to_owned())
}

/// `/proc/<pid>/stat` から親 PID を読む。comm は括弧で囲まれ空白や括弧を
/// 含みうるため、**最後の `)`** より後を field 列として読む。
#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `/proc/<pid>/environ` から herdr の 2 key だけを取り出す。
/// **両方揃わない process は identity ではない** (`None` — 探索は続く)。
#[cfg(target_os = "linux")]
fn herdr_identity_of(pid: i32) -> Option<HerdrIdentity> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let mut pane = None;
    let mut herdr_socket = None;
    for entry in environ.split(|byte| *byte == 0) {
        let Ok(entry) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(value) = entry.strip_prefix("HERDR_PANE_ID=")
            && !value.is_empty()
        {
            pane = Some(value.to_owned());
        } else if let Some(value) = entry.strip_prefix("HERDR_SOCKET_PATH=")
            && !value.is_empty()
        {
            herdr_socket = Some(value.to_owned());
        }
    }
    Some(HerdrIdentity {
        pane: pane?,
        herdr_socket: herdr_socket?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_resolver<'a>(
        parents: &'a [(i32, i32)],
        identities: &'a [(i32, &'a str, &'a str)],
    ) -> (
        impl Fn(i32) -> Option<i32> + 'a,
        impl Fn(i32) -> Option<HerdrIdentity> + 'a,
    ) {
        let parent_of = move |pid: i32| {
            parents
                .iter()
                .find(|(child, _)| *child == pid)
                .map(|(_, parent)| *parent)
        };
        let identity_of = move |pid: i32| {
            identities
                .iter()
                .find(|(candidate, _, _)| *candidate == pid)
                .map(|(_, pane, socket)| HerdrIdentity {
                    pane: (*pane).to_owned(),
                    herdr_socket: (*socket).to_owned(),
                })
        };
        (parent_of, identity_of)
    }

    #[test]
    fn the_nearest_ancestor_with_a_herdr_identity_wins() {
        // mcp(100) → launcher(90) → agent(80, identity あり) → shell(70)
        let (parents, identities) = table_resolver(
            &[(100, 90), (90, 80), (80, 70)],
            &[(80, "w1:p7", "/run/herdr/herdr.sock")],
        );
        let pane =
            resolve_with(100, Path::new("/run/herdr/herdr.sock"), parents, identities).unwrap();
        assert_eq!(pane, "w1:p7");
    }

    #[test]
    fn a_caller_from_another_herdr_session_is_refused() {
        let (parents, identities) = table_resolver(
            &[(100, 80)],
            &[(80, "w1:p7", "/run/herdr/sessions/review/herdr.sock")],
        );
        let error =
            resolve_with(100, Path::new("/run/herdr/herdr.sock"), parents, identities).unwrap_err();
        assert!(error.contains("別の herdr"), "{error}");
    }

    #[test]
    fn a_chain_without_identity_fails_closed() {
        let (parents, identities) = table_resolver(&[(100, 90), (90, 1)], &[]);
        let error =
            resolve_with(100, Path::new("/run/herdr/herdr.sock"), parents, identities).unwrap_err();
        assert!(error.contains("見つかりません"), "{error}");
    }

    #[test]
    fn a_cyclic_or_vanished_parent_chain_terminates() {
        // PID 再利用で親子が循環しても停止する。
        let (parents, identities) = table_resolver(&[(100, 90), (90, 100)], &[]);
        assert!(
            resolve_with(100, Path::new("/run/herdr/herdr.sock"), parents, identities).is_err()
        );
        // 親が観測できない場合も fail closed。
        let (parents, identities) = table_resolver(&[], &[]);
        assert!(
            resolve_with(100, Path::new("/run/herdr/herdr.sock"), parents, identities).is_err()
        );
    }

    /// pane key だけの祖先は identity ではない — pane id は session 間で
    /// 衝突しうるため、socket で帰属を証明できない bind はしない。
    /// (Linux 実装は `herdr_identity_of` が両 key 必須で `None` を返す。)
    #[test]
    fn a_pane_key_without_its_socket_never_binds() {
        // 片欠けの祖先 (90) は素通りし、両 key の祖先 (80) が bind する。
        let identity_of = |pid: i32| match pid {
            80 => Some(HerdrIdentity {
                pane: "w1:p7".into(),
                herdr_socket: "/run/herdr/herdr.sock".into(),
            }),
            _ => None,
        };
        let (parents, _) = table_resolver(&[(100, 90), (90, 80)], &[]);
        let pane = resolve_with(
            100,
            Path::new("/run/herdr/herdr.sock"),
            parents,
            identity_of,
        )
        .unwrap();
        assert_eq!(pane, "w1:p7");
    }
}
