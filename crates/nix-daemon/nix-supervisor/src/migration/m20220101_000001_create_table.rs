// SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Build::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Build::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Build::Name).string().not_null())
                    .col(ColumnDef::new(Build::MachineName).string().not_null())
                    .col(ColumnDef::new(Build::Start).date_time().not_null())
                    .col(
                        ColumnDef::new(Build::StartPhase)
                            .json()
                            .not_null()
                            .default("[]"),
                    )
                    .col(ColumnDef::new(Build::End).date_time().null())
                    .col(ColumnDef::new(Build::TempLogPath).string().null())
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(format!(
                        "{}_{}",
                        Build::Table.to_string(),
                        Build::Name.to_string()
                    ))
                    .table(Build::Table)
                    .col(Build::Name)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(format!(
                        "{}_{}",
                        Build::Table.to_string(),
                        Build::Start.to_string()
                    ))
                    .table(Build::Table)
                    .col(Build::Start)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Build::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Build {
    #[sea_orm(iden = "builds")]
    Table,
    Id,
    Name,
    MachineName,
    Start,
    StartPhase,
    End,
    TempLogPath,
}
