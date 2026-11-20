use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "enrollments")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub exam_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub oj_user_id: i64,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::exam::Entity",
        from = "Column::ExamId",
        to = "super::exam::Column::ExamId"
    )]
    Exam,
}

impl ActiveModelBehavior for ActiveModel {}
