use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "commands")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub command_id: String,
    pub exam_id: String,
    pub oj_user_id: i64,
    pub connection_id: String,
    pub name: String,
    pub effective_at: String,
    pub created_at: String,
    pub lifecycle: String,
    pub result_status: Option<String>,
    pub result_error_code: Option<String>,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
