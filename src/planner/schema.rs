use core::fmt;

use crate::planner::types::DataType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnId(pub u32);

impl fmt::Display for ColumnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Debug, Default)]
pub struct ColumnIdGenerator {
    next: u32,
}

impl ColumnIdGenerator {
    pub fn next_id(&mut self) -> ColumnId {
        let id = ColumnId(self.next);
        self.next += 1;
        id
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub id: ColumnId,
    pub qualifier: Option<String>,
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl Field {
    pub fn qualified_name(&self) -> String {
        match &self.qualifier {
            Some(q) => format!("{q}.{}", self.name),
            None => self.name.clone(),
        }
    }

    pub fn with_nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }
}

#[derive(Debug, PartialEq, Clone, Default)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn into_fields(self) -> Vec<Field> {
        self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn field(&self, index: usize) -> &Field {
        &self.fields[index]
    }

    pub fn index_of(&self, col_id: ColumnId) -> Option<usize> {
        self.fields.iter().position(|f| f.id == col_id)
    }

    pub fn contains(&self, col_id: ColumnId) -> bool {
        self.index_of(col_id).is_some()
    }

    pub fn ids(&self) -> impl Iterator<Item = ColumnId> + '_ {
        self.fields.iter().map(|f| f.id)
    }

    pub fn concat(&self, other: &Schema) -> Schema {
        let mut fields = self.fields.clone();
        fields.extend(other.fields.iter().cloned());
        Schema { fields }
    }

    pub fn with_nullable(&self, nullable: bool) -> Schema {
        Schema {
            fields: self
                .fields
                .iter()
                .map(|f| f.clone().with_nullable(f.nullable || nullable))
                .collect(),
        }
    }

    pub fn column_names(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.name.clone()).collect()
    }
}

impl fmt::Display for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}:{}", field.qualified_name(), field.data_type)?;
        }
        f.write_str("]")
    }
}
