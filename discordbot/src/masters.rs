//! masters シートの読み込みと進行度の遷移ルール。
//!
//! masters シートは A列=key, B列=value, C列=補足 の縦持ち。
//! - status 行: C列に役割 (initial / in_progress / done / suspended / discarded)。省略時は行順で割当
//! - assignee 行: C列に Discord ID
//! - rename 行: B列=旧名, C列=新名 (/sync tags で Forum タグと tickets に反映)

use anyhow::{Result, bail};

use crate::config::Config;
use crate::sheets::{Sheets, quote_sheet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusRole {
    Initial,
    InProgress,
    Done,
    Suspended,
    Discarded,
}

impl StatusRole {
    pub const ALL: [StatusRole; 5] = [
        StatusRole::Initial,
        StatusRole::InProgress,
        StatusRole::Done,
        StatusRole::Suspended,
        StatusRole::Discarded,
    ];

    pub fn key(self) -> &'static str {
        match self {
            StatusRole::Initial => "initial",
            StatusRole::InProgress => "in_progress",
            StatusRole::Done => "done",
            StatusRole::Suspended => "suspended",
            StatusRole::Discarded => "discarded",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.key() == s.trim())
    }

    /// 完了・中断・破棄は Forum Post を Locked にして残す
    pub fn locks(self) -> bool {
        matches!(
            self,
            StatusRole::Done | StatusRole::Suspended | StatusRole::Discarded
        )
    }

    pub fn color(self) -> u32 {
        match self {
            StatusRole::Initial => 0x95a5a6,
            StatusRole::InProgress => 0x3498db,
            StatusRole::Done => 0x2ecc71,
            StatusRole::Suspended => 0xe67e22,
            StatusRole::Discarded => 0x7f8c8d,
        }
    }
}

/// 未着手→着手中→完了。完了→着手中の差戻し (修正依頼) 可。中断・破棄はどこからでも可
pub fn can_transition(from: StatusRole, to: StatusRole) -> bool {
    use StatusRole::*;
    if from == to {
        return false;
    }
    matches!(
        (from, to),
        (Initial, InProgress) | (InProgress, Done) | (Done, InProgress)
    ) || matches!(to, Suspended | Discarded)
}

#[derive(Debug, Clone)]
pub struct Status {
    pub name: String,
    pub role: StatusRole,
}

#[derive(Debug, Clone)]
pub struct Assignee {
    pub name: String,
    pub discord_id: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Masters {
    pub statuses: Vec<Status>,
    pub categories: Vec<String>,
    pub assignees: Vec<Assignee>,
    pub priorities: Vec<String>,
    pub renames: Vec<(String, String)>,
}

impl Masters {
    pub async fn load(sheets: &Sheets, cfg: &Config) -> Result<Self> {
        let rows = sheets
            .get(&format!("{}!A:C", quote_sheet(&cfg.sheets.masters_sheet)))
            .await?;
        Self::parse(&rows, cfg)
    }

    /// masters シート A:C の値から組み立てる (GAS からの送信内容にも使う)
    pub fn parse(rows: &[Vec<String>], cfg: &Config) -> Result<Self> {
        let k = &cfg.masters;
        let mut m = Masters::default();
        let mut status_rows: Vec<(String, Option<StatusRole>)> = Vec::new();
        for row in rows {
            let cell = |i: usize| row.get(i).map(|s| s.trim()).unwrap_or("");
            let (key, value, extra) = (cell(0), cell(1), cell(2));
            if value.is_empty() {
                continue;
            }
            if key == k.status_key {
                let role = StatusRole::from_key(extra);
                if role.is_none() && !extra.is_empty() {
                    bail!(
                        "masters の status「{value}」の役割「{extra}」は不明です (initial / in_progress / done / suspended / discarded のいずれか。チェック待ち (review) は廃止しました)"
                    );
                }
                status_rows.push((value.into(), role));
            } else if key == k.category_key {
                m.categories.push(value.into());
            } else if key == k.priority_key {
                m.priorities.push(value.into());
            } else if key == k.assignee_key {
                m.assignees.push(Assignee {
                    name: value.into(),
                    discord_id: extra.parse().ok(),
                });
            } else if key == k.rename_key && !extra.is_empty() {
                m.renames.push((value.into(), extra.into()));
            }
        }
        // 役割が明示されていなければ行順 (未着手, 着手中, 完了, 中断, 破棄) で割り当てる
        for (i, (name, role)) in status_rows.into_iter().enumerate() {
            let role = match role.or_else(|| StatusRole::ALL.get(i).copied()) {
                Some(r) => r,
                None => bail!(
                    "masters の status「{name}」の役割が決められません (C列に役割を指定してください)"
                ),
            };
            m.statuses.push(Status { name, role });
        }
        for role in StatusRole::ALL {
            if !m.statuses.iter().any(|s| s.role == role) {
                bail!("masters に役割 {} の status がありません", role.key());
            }
        }
        if m.categories.is_empty() || m.priorities.is_empty() {
            bail!("masters に category または priority がありません");
        }
        Ok(m)
    }

    pub fn status_by_role(&self, role: StatusRole) -> &Status {
        self.statuses.iter().find(|s| s.role == role).unwrap()
    }

    pub fn role_of(&self, status_name: &str) -> Option<StatusRole> {
        self.statuses
            .iter()
            .find(|s| s.name == status_name)
            .map(|s| s.role)
    }

    pub fn assignee_by_id(&self, id: u64) -> Option<&Assignee> {
        self.assignees.iter().find(|a| a.discord_id == Some(id))
    }

    pub fn assignee_by_name(&self, name: &str) -> Option<&Assignee> {
        self.assignees.iter().find(|a| a.name == name)
    }

    pub fn priority_rank(&self, p: &str) -> usize {
        self.priorities
            .iter()
            .position(|x| x == p)
            .unwrap_or(usize::MAX)
    }

    /// Forum タグにする名前 (進行度, 種類, 優先度, 担当者の順。上限を超える担当者はタグ化しない)
    pub fn tag_names(&self, max: usize) -> Vec<String> {
        let mut names: Vec<String> = self.statuses.iter().map(|s| s.name.clone()).collect();
        names.extend(self.categories.iter().cloned());
        names.extend(self.priorities.iter().cloned());
        names.extend(self.assignees.iter().map(|a| a.name.clone()));
        let mut seen = std::collections::HashSet::new();
        names.retain(|n| seen.insert(n.clone()));
        names.truncate(max);
        names
    }

    /// masters シートに担当者を追記する
    pub async fn append_assignee(sheets: &Sheets, cfg: &Config, name: &str, id: u64) -> Result<()> {
        sheets
            .append_row(
                &cfg.sheets.masters_sheet,
                1,
                vec![
                    cfg.masters.assignee_key.clone(),
                    name.into(),
                    id.to_string(),
                ],
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::StatusRole::*;
    use super::*;

    #[test]
    fn transitions() {
        assert!(can_transition(Initial, InProgress));
        assert!(can_transition(InProgress, Done));
        assert!(can_transition(Done, InProgress));
        assert!(can_transition(Done, Discarded));
        assert!(can_transition(Initial, Suspended));
        assert!(!can_transition(Initial, Done));
        assert!(!can_transition(Suspended, InProgress));
        assert!(!can_transition(Suspended, Suspended));
    }

    fn rows(spec: &[(&str, &str, &str)]) -> Vec<Vec<String>> {
        spec.iter()
            .map(|(k, v, c)| vec![k.to_string(), v.to_string(), c.to_string()])
            .collect()
    }

    fn sample() -> Vec<(&'static str, &'static str, &'static str)> {
        let mut spec = vec![
            ("status", "未着手", ""),
            ("status", "着手中", ""),
            ("status", "完了", ""),
            ("status", "中断", ""),
            ("status", "破棄", ""),
        ];
        for c in ["実装", "アセット", "エンジン", "演出", "企画", "バグ・違和感"] {
            spec.push(("category", c, ""));
        }
        for p in ["緊急", "高", "中", "低"] {
            spec.push(("priority", p, ""));
        }
        for (a, id) in [("A", "1"), ("B", "2"), ("C", "3"), ("D", "4"), ("E", "5")] {
            spec.push(("assignee", a, id));
        }
        spec
    }

    #[test]
    fn tags_fit_forum_limit() {
        let cfg = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        let m = Masters::parse(&rows(&sample()), &cfg).unwrap();
        assert_eq!(m.status_by_role(Done).name, "完了");
        let tags = m.tag_names(20);
        // 進行度5 + 種類6 + 優先度4 + 担当者5 = 20
        assert_eq!(tags.len(), 20);
        assert_eq!(&tags[11..15], ["緊急", "高", "中", "低"]);
        assert_eq!(tags[19], "E");
    }

    #[test]
    fn review_role_is_rejected() {
        let cfg = Config::load(concat!(env!("CARGO_MANIFEST_DIR"), "/config.toml")).unwrap();
        let mut spec = sample();
        spec.insert(2, ("status", "チェック待ち", "review"));
        assert!(Masters::parse(&rows(&spec), &cfg).is_err());
    }
}
