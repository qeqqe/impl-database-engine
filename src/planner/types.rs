use core::fmt;

use crate::sql::SqlDataType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Null,
    Boolean,
    Integer,
    Float,
    Text,
    Json,
    Timestamp,
    Uuid,
}

impl DataType {
    pub fn is_null(&self) -> bool {
        self == &DataType::Null
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, DataType::Integer | DataType::Float)
    }

    pub fn is_textual(&self) -> bool {
        matches!(self, DataType::Text | DataType::Json | DataType::Uuid)
    }

    pub fn is_boolean(&self) -> bool {
        self == &DataType::Boolean
    }

    pub fn numeric_result(self, other: DataType) -> Option<DataType> {
        match (self, other) {
            (DataType::Null, DataType::Null) => Some(DataType::Null),
            (DataType::Null, t) | (t, DataType::Null) if t.is_numeric() => Some(t),
            (DataType::Integer, DataType::Integer) => Some(DataType::Integer),
            (a, b) if a.is_numeric() && b.is_numeric() => Some(DataType::Float),
            _ => None,
        }
    }

    pub fn is_comparable_with(self, other: DataType) -> bool {
        self.is_null()
            || other.is_null()
            || self == other
            || (self.is_numeric() && other.is_numeric())
            || (self.is_textual() && other.is_textual())
    }

    pub fn can_assign_to(self, target: DataType) -> bool {
        self == target
            || self.is_null()
            || (self == DataType::Integer && target == DataType::Float)
            || (self == DataType::Text && target.is_textual())
    }

    pub fn unify(self, other: DataType) -> Option<DataType> {
        match (self, other) {
            (a, b) if a == b => Some(a),
            (DataType::Null, t) | (t, DataType::Null) => Some(t),
            (a, b) if a.is_numeric() && b.is_numeric() => Some(DataType::Float),
            (a, b) if a.is_textual() && b.is_textual() => Some(DataType::Text),
            _ => None,
        }
    }
}

impl From<&SqlDataType> for DataType {
    fn from(value: &SqlDataType) -> Self {
        match value {
            SqlDataType::Text => DataType::Text,
            SqlDataType::Integer => DataType::Integer,
            SqlDataType::Float => DataType::Float,
            SqlDataType::Boolean => DataType::Boolean,
            SqlDataType::Json => DataType::Json,
            SqlDataType::Timestamp => DataType::Timestamp,
            SqlDataType::Uuid => DataType::Uuid,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            DataType::Null => "NULL",
            DataType::Boolean => "BOOLEAN",
            DataType::Integer => "INTEGER",
            DataType::Float => "FLOAT",
            DataType::Text => "TEXT",
            DataType::Json => "JSON",
            DataType::Timestamp => "TIMESTAMP",
            DataType::Uuid => "UUID",
        };
        f.write_str(name)
    }
}
