/*
 * Created on Mon Aug 03 2020
 *
 * This file is a part of Skytable
 * Skytable (formerly known as TerrabaseDB or Skybase) is a free and open-source
 * NoSQL database written by Sayan Nandan ("the Author") with the
 * vision to provide flexibility in data modelling without compromising
 * on performance, queryability or scalability.
 *
 * Copyright (c) 2020, Sayan Nandan <ohsayan@outlook.com>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program. If not, see <https://www.gnu.org/licenses/>.
 *
*/

//! # The Query Engine

use crate::corestore::memstore::DdlError;
use crate::corestore::Corestore;
use crate::dbnet::connection::prelude::*;
use crate::protocol::responses;
use crate::protocol::Element;
use crate::{actions, admin};
use bytes::Bytes;
mod ddl;
mod inspect;
pub mod parser;
#[cfg(test)]
mod tests;

use std::vec::IntoIter;
pub type ActionIter = IntoIter<Bytes>;

macro_rules! gen_constants_and_matches {
    ($con:ident, $buf:ident, $db:ident, $($action:ident => $fns:expr),*) => {
        mod tags {
            //! This module is a collection of tags/strings used for evaluating queries
            //! and responses
            $(
                pub const $action: &[u8] = stringify!($action).as_bytes();
            )*
        }
        let mut first = match $buf.next() {
            Some(frst) => frst.to_vec(),
            None => return $con.write_response(responses::groups::PACKET_ERR).await,
        };
        first.make_ascii_uppercase();
        match first.as_ref() {
            $(
                tags::$action => $fns($db, $con, $buf).await?,
            )*
            _ => {
                return $con.write_response(responses::groups::UNKNOWN_ACTION).await;
            }
        }
    };
}

macro_rules! swap_entity {
    ($con:expr, $handle:expr, $entity:expr) => {
        match parser::get_query_entity(&$entity) {
            Ok(e) => match $handle.swap_entity(e) {
                Ok(()) => $con.write_response(responses::groups::OKAY).await?,
                Err(DdlError::ObjectNotFound) => {
                    $con.write_response(responses::groups::CONTAINER_NOT_FOUND)
                        .await?
                }
                Err(DdlError::DefaultNotFound) => {
                    $con.write_response(responses::groups::DEFAULT_UNSET)
                        .await?
                }
                Err(_) => unsafe {
                    // we know Corestore::swap_entity doesn't return anything else
                    impossible!()
                },
            },
            Err(e) => $con.write_response(e).await?,
        }
    };
}

/// Execute a simple(*) query
pub async fn execute_simple<T, Strm>(
    db: &mut Corestore,
    con: &mut T,
    buf: Element,
) -> std::io::Result<()>
where
    T: ProtocolConnectionExt<Strm>,
    Strm: AsyncReadExt + AsyncWriteExt + Unpin + Send + Sync,
{
    let buf = match buf {
        Element::AnyArray(a) => a,
        _ => return con.write_response(responses::groups::WRONGTYPE_ERR).await,
    };
    let mut buf = buf.into_iter();
    gen_constants_and_matches!(
        con, buf, db,
        GET => actions::get::get,
        SET => actions::set::set,
        UPDATE => actions::update::update,
        DEL => actions::del::del,
        HEYA => actions::heya::heya,
        EXISTS => actions::exists::exists,
        MSET => actions::mset::mset,
        MGET => actions::mget::mget,
        MUPDATE => actions::mupdate::mupdate,
        SSET => actions::strong::sset,
        SDEL => actions::strong::sdel,
        SUPDATE => actions::strong::supdate,
        DBSIZE => actions::dbsize::dbsize,
        FLUSHDB => actions::flushdb::flushdb,
        USET => actions::uset::uset,
        KEYLEN => actions::keylen::keylen,
        MKSNAP => admin::mksnap::mksnap,
        LSKEYS => actions::lskeys::lskeys,
        POP => actions::pop::pop,
        CREATE => ddl::create,
        DROP => ddl::ddl_drop,
        USE => self::entity_swap,
        INSPECT => inspect::inspect,
        MPOP => actions::mpop::mpop
    );
    Ok(())
}

action! {
    /// Handle `use <entity>` like queries
    fn entity_swap(handle: &mut Corestore, con: &mut T, mut act: ActionIter) {
        err_if_len_is!(act, con, not 1);
        let entity = unsafe {
            // SAFETY: Already checked len
            act.next().unsafe_unwrap()
        };
        swap_entity!(con, handle, entity);
        Ok(())
    }
}
