//! Pure structural diff between two [`crate::schema::Schema`] snapshots.
//!
//! Convention: `diff(against, current)` answers "what changed in `current`
//! compared to `against`?". So a column present in `current` but not in
//! `against` is `added_columns`. Mirrors `git diff <main> <feat>`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::schema::{Column, ForeignKey, Index, Schema, Table};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SchemaDiff {
    pub added_tables: Vec<Table>,
    pub removed_tables: Vec<Table>,
    pub changed_tables: Vec<TableDiff>,
}

impl SchemaDiff {
    pub fn is_empty(&self) -> bool {
        self.added_tables.is_empty()
            && self.removed_tables.is_empty()
            && self.changed_tables.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableDiff {
    pub schema: String,
    pub name: String,
    pub added_columns: Vec<Column>,
    pub removed_columns: Vec<Column>,
    pub changed_columns: Vec<ColumnChange>,
    pub added_foreign_keys: Vec<ForeignKey>,
    pub removed_foreign_keys: Vec<ForeignKey>,
    pub added_indexes: Vec<Index>,
    pub removed_indexes: Vec<Index>,
    /// True iff the PRIMARY KEY columns changed.
    pub primary_key_changed: bool,
    pub old_primary_key: Vec<String>,
    pub new_primary_key: Vec<String>,
}

impl TableDiff {
    fn is_empty(&self) -> bool {
        self.added_columns.is_empty()
            && self.removed_columns.is_empty()
            && self.changed_columns.is_empty()
            && self.added_foreign_keys.is_empty()
            && self.removed_foreign_keys.is_empty()
            && self.added_indexes.is_empty()
            && self.removed_indexes.is_empty()
            && !self.primary_key_changed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnChange {
    pub name: String,
    pub old: ColumnFacts,
    pub new: ColumnFacts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnFacts {
    pub data_type: String,
    pub is_nullable: bool,
    pub default: Option<String>,
}

impl From<&Column> for ColumnFacts {
    fn from(c: &Column) -> Self {
        Self {
            data_type: c.data_type.clone(),
            is_nullable: c.is_nullable,
            default: c.default.clone(),
        }
    }
}

/// Computes the structural diff. `against` is the baseline (e.g. `main`);
/// `current` is the branch the user is inspecting.
pub fn diff(against: &Schema, current: &Schema) -> SchemaDiff {
    let by_key = |s: &Schema| -> BTreeMap<(String, String), Table> {
        s.tables
            .iter()
            .map(|t| ((t.schema.clone(), t.name.clone()), t.clone()))
            .collect()
    };
    let a = by_key(against);
    let b = by_key(current);

    let mut added_tables = Vec::new();
    let mut removed_tables = Vec::new();
    let mut changed_tables = Vec::new();

    for (key, t) in &b {
        if !a.contains_key(key) {
            added_tables.push(t.clone());
        }
    }
    for (key, t) in &a {
        if !b.contains_key(key) {
            removed_tables.push(t.clone());
        }
    }
    for (key, ta) in &a {
        if let Some(tb) = b.get(key) {
            let td = diff_table(ta, tb);
            if !td.is_empty() {
                changed_tables.push(td);
            }
        }
    }

    SchemaDiff {
        added_tables,
        removed_tables,
        changed_tables,
    }
}

fn diff_table(a: &Table, b: &Table) -> TableDiff {
    let cols_a: BTreeMap<&str, &Column> = a.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let cols_b: BTreeMap<&str, &Column> = b.columns.iter().map(|c| (c.name.as_str(), c)).collect();

    let added_columns: Vec<Column> = cols_b
        .iter()
        .filter(|(n, _)| !cols_a.contains_key(*n))
        .map(|(_, c)| (*c).clone())
        .collect();
    let removed_columns: Vec<Column> = cols_a
        .iter()
        .filter(|(n, _)| !cols_b.contains_key(*n))
        .map(|(_, c)| (*c).clone())
        .collect();

    let mut changed_columns = Vec::new();
    for (name, ca) in &cols_a {
        if let Some(cb) = cols_b.get(name) {
            let fa: ColumnFacts = (*ca).into();
            let fb: ColumnFacts = (*cb).into();
            if fa != fb {
                changed_columns.push(ColumnChange {
                    name: (*name).to_string(),
                    old: fa,
                    new: fb,
                });
            }
        }
    }

    let fks_a: BTreeMap<&str, &ForeignKey> =
        a.foreign_keys.iter().map(|f| (f.name.as_str(), f)).collect();
    let fks_b: BTreeMap<&str, &ForeignKey> =
        b.foreign_keys.iter().map(|f| (f.name.as_str(), f)).collect();
    let added_foreign_keys: Vec<ForeignKey> = fks_b
        .iter()
        .filter(|(n, _)| !fks_a.contains_key(*n))
        .map(|(_, f)| (*f).clone())
        .collect();
    let removed_foreign_keys: Vec<ForeignKey> = fks_a
        .iter()
        .filter(|(n, _)| !fks_b.contains_key(*n))
        .map(|(_, f)| (*f).clone())
        .collect();

    let idxs_a: BTreeMap<&str, &Index> = a.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
    let idxs_b: BTreeMap<&str, &Index> = b.indexes.iter().map(|i| (i.name.as_str(), i)).collect();
    let added_indexes: Vec<Index> = idxs_b
        .iter()
        .filter(|(n, _)| !idxs_a.contains_key(*n))
        .map(|(_, i)| (*i).clone())
        .collect();
    let removed_indexes: Vec<Index> = idxs_a
        .iter()
        .filter(|(n, _)| !idxs_b.contains_key(*n))
        .map(|(_, i)| (*i).clone())
        .collect();

    let primary_key_changed = a.primary_key != b.primary_key;

    TableDiff {
        schema: a.schema.clone(),
        name: a.name.clone(),
        added_columns,
        removed_columns,
        changed_columns,
        added_foreign_keys,
        removed_foreign_keys,
        added_indexes,
        removed_indexes,
        primary_key_changed,
        old_primary_key: a.primary_key.clone(),
        new_primary_key: b.primary_key.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Column {
        Column {
            name: name.into(),
            data_type: ty.into(),
            is_nullable: true,
            default: None,
        }
    }

    fn tbl(name: &str, cols: Vec<Column>) -> Table {
        Table {
            schema: "public".into(),
            name: name.into(),
            columns: cols,
            primary_key: vec![],
            foreign_keys: vec![],
            indexes: vec![],
        }
    }

    #[test]
    fn no_op_when_identical() {
        let s = Schema {
            tables: vec![tbl("users", vec![col("id", "integer")])],
        };
        assert!(diff(&s, &s).is_empty());
    }

    #[test]
    fn detects_added_and_removed_tables() {
        let a = Schema {
            tables: vec![tbl("users", vec![])],
        };
        let b = Schema {
            tables: vec![tbl("users", vec![]), tbl("posts", vec![])],
        };
        let d = diff(&a, &b);
        assert_eq!(d.added_tables.len(), 1);
        assert_eq!(d.added_tables[0].name, "posts");
        assert!(d.removed_tables.is_empty());

        let rev = diff(&b, &a);
        assert_eq!(rev.removed_tables.len(), 1);
        assert_eq!(rev.removed_tables[0].name, "posts");
    }

    #[test]
    fn detects_added_and_removed_columns() {
        let a = Schema {
            tables: vec![tbl("users", vec![col("id", "integer")])],
        };
        let b = Schema {
            tables: vec![tbl(
                "users",
                vec![col("id", "integer"), col("name", "text")],
            )],
        };
        let d = diff(&a, &b);
        assert_eq!(d.changed_tables.len(), 1);
        let td = &d.changed_tables[0];
        assert_eq!(td.added_columns.len(), 1);
        assert_eq!(td.added_columns[0].name, "name");
        assert!(td.removed_columns.is_empty());
    }

    #[test]
    fn detects_column_type_change() {
        let a = Schema {
            tables: vec![tbl("users", vec![col("id", "integer")])],
        };
        let mut col_changed = col("id", "bigint");
        col_changed.is_nullable = false;
        let b = Schema {
            tables: vec![tbl("users", vec![col_changed])],
        };
        let d = diff(&a, &b);
        let td = &d.changed_tables[0];
        assert_eq!(td.changed_columns.len(), 1);
        let cc = &td.changed_columns[0];
        assert_eq!(cc.old.data_type, "integer");
        assert_eq!(cc.new.data_type, "bigint");
        assert_eq!(cc.old.is_nullable, true);
        assert_eq!(cc.new.is_nullable, false);
    }

    #[test]
    fn detects_primary_key_change() {
        let mut a_tbl = tbl("users", vec![col("id", "integer")]);
        a_tbl.primary_key = vec!["id".into()];
        let a = Schema { tables: vec![a_tbl] };
        let mut b_tbl = tbl("users", vec![col("id", "integer")]);
        b_tbl.primary_key = vec![];
        let b = Schema { tables: vec![b_tbl] };
        let d = diff(&a, &b);
        let td = &d.changed_tables[0];
        assert!(td.primary_key_changed);
        assert_eq!(td.old_primary_key, vec!["id"]);
        assert!(td.new_primary_key.is_empty());
    }

    #[test]
    fn detects_added_and_removed_indexes() {
        let mut a_tbl = tbl("users", vec![col("id", "integer")]);
        a_tbl.indexes = vec![Index {
            name: "users_pkey".into(),
            columns: vec!["id".into()],
            is_unique: true,
            is_primary: true,
        }];
        let a = Schema { tables: vec![a_tbl] };
        let mut b_tbl = tbl("users", vec![col("id", "integer"), col("name", "text")]);
        b_tbl.indexes = vec![
            Index {
                name: "users_pkey".into(),
                columns: vec!["id".into()],
                is_unique: true,
                is_primary: true,
            },
            Index {
                name: "users_name_idx".into(),
                columns: vec!["name".into()],
                is_unique: false,
                is_primary: false,
            },
        ];
        let b = Schema { tables: vec![b_tbl] };
        let d = diff(&a, &b);
        let td = &d.changed_tables[0];
        assert_eq!(td.added_indexes.len(), 1);
        assert_eq!(td.added_indexes[0].name, "users_name_idx");
        assert!(td.removed_indexes.is_empty());
    }

    #[test]
    fn unchanged_tables_are_not_in_changed_list() {
        let a = Schema {
            tables: vec![
                tbl("users", vec![col("id", "integer")]),
                tbl("posts", vec![col("id", "integer")]),
            ],
        };
        let mut b = a.clone();
        // Add a column to `posts` only.
        b.tables[1].columns.push(col("title", "text"));
        let d = diff(&a, &b);
        assert_eq!(d.changed_tables.len(), 1);
        assert_eq!(d.changed_tables[0].name, "posts");
    }
}
