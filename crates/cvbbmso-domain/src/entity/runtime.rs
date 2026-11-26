use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "student_runtimes")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub exam_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub oj_user_id: i64,
    pub device_id: String,
    pub instance_id: String,
    pub connection_id: Option<String>,
    pub connected: bool,
    pub execution: String,
    pub recording: String,
    pub last_seen_at: String,
    pub ack_event_through: i64,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
