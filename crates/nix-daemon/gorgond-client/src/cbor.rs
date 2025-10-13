// SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
//
// SPDX-License-Identifier: EUPL-1.2

pub mod uuid {
    use minicbor::{
        data::Tag,
        decode::{self, Decoder},
        encode::{self, Encoder, Write},
    };
    use uuid::Uuid;

    pub const TAG: Tag = Tag::new(37);

    pub fn encode<Ctx, W: Write>(
        v: &Uuid,
        e: &mut Encoder<W>,
        _: &mut Ctx,
    ) -> Result<(), encode::Error<W::Error>> {
        e.tag(TAG)?;
        e.bytes(v.as_bytes())?;
        Ok(())
    }
    pub fn decode<Ctx>(d: &mut Decoder, _: &mut Ctx) -> Result<Uuid, decode::Error> {
        match d.tag()? {
            TAG => d
                .bytes()
                .and_then(|b| Uuid::from_slice(b).map_err(decode::Error::custom)),
            other => Err(decode::Error::tag_mismatch(other)),
        }
    }

    pub fn nil() -> Option<Uuid> {
        Some(Uuid::nil())
    }
    pub fn is_nil(v: &Uuid) -> bool {
        v.is_nil()
    }

    pub mod opt {
        use super::*;

        pub fn encode<Ctx, W: Write>(
            v: &Option<Uuid>,
            e: &mut Encoder<W>,
            ctx: &mut Ctx,
        ) -> Result<(), encode::Error<W::Error>> {
            super::encode(&v.unwrap_or_default(), e, ctx)
        }
        pub fn decode<Ctx>(d: &mut Decoder, ctx: &mut Ctx) -> Result<Option<Uuid>, decode::Error> {
            Some(super::decode(d, ctx)).transpose()
        }

        pub fn nil() -> Option<Option<Uuid>> {
            Some(None)
        }
        pub fn is_nil(v: &Option<Uuid>) -> bool {
            v.is_none()
        }
    }
}

pub mod timestamp {
    use chrono::prelude::*;
    use minicbor::{
        data::IanaTag,
        decode::{self, Decoder},
        encode::{self, Encoder, Write},
    };

    pub fn encode<Ctx, W: Write>(
        v: &DateTime<Utc>,
        e: &mut Encoder<W>,
        _: &mut Ctx,
    ) -> Result<(), encode::Error<W::Error>> {
        e.tag(IanaTag::Timestamp)?;
        e.i64(v.timestamp())?;
        Ok(())
    }
    pub fn decode<Ctx>(d: &mut Decoder, _: &mut Ctx) -> Result<DateTime<Utc>, decode::Error> {
        match d.tag()? {
            tag if tag == IanaTag::Timestamp.tag() => d.i64().and_then(|ts| {
                DateTime::from_timestamp(ts, 0).ok_or(decode::Error::message("invalid timestamp"))
            }),
            other => Err(decode::Error::tag_mismatch(other)),
        }
    }

    pub fn nil() -> Option<DateTime<Utc>> {
        Some(DateTime::UNIX_EPOCH)
    }
    pub fn is_nil(v: &DateTime<Utc>) -> bool {
        *v == DateTime::UNIX_EPOCH
    }

    pub mod opt {
        use super::*;

        pub fn encode<Ctx, W: Write>(
            v: &Option<DateTime<Utc>>,
            e: &mut Encoder<W>,
            ctx: &mut Ctx,
        ) -> Result<(), encode::Error<W::Error>> {
            super::encode(&v.unwrap_or_default(), e, ctx)
        }
        pub fn decode<Ctx>(
            d: &mut Decoder,
            ctx: &mut Ctx,
        ) -> Result<Option<DateTime<Utc>>, decode::Error> {
            Some(super::decode(d, ctx)).transpose()
        }

        pub fn nil() -> Option<Option<DateTime<Utc>>> {
            Some(None)
        }
        pub fn is_nil(v: &Option<DateTime<Utc>>) -> bool {
            v.is_none()
        }
    }
}
