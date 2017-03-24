use super::schema::timers;

#[derive(Queryable)]
pub struct Timer {
    pub id: Option<i64>,
    pub at: i64,
    pub whom: String,
    pub operation: String,
}

#[derive(Insertable)]
#[table_name="timers"]
pub struct NewTimer<'a> {
    pub at: i64,
    pub whom: &'a str,
    pub operation: &'a str,
}

