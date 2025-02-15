// Copyright (c) 2023-2024 Retake, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use pgrx::{pg_sys::ItemPointerData, *};
use tantivy::postings::Postings;
use tantivy::schema::IndexRecordOption;
use tantivy::{DocSet, Term};

use super::storage::block::CLEANUP_LOCK;
use crate::index::merge_policy::MergeLock;
use crate::index::reader::index::SearchIndexReader;
use crate::index::writer::index::SearchIndexWriter;
use crate::index::{BlockDirectoryType, WriterResources};

#[repr(C)]
pub struct BulkDeleteData {
    stats: pg_sys::IndexBulkDeleteResult,
    pub cleanup_lock: pg_sys::Buffer,
}

#[pg_guard]
pub extern "C" fn ambulkdelete(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    callback: pg_sys::IndexBulkDeleteCallback,
    callback_state: *mut ::std::os::raw::c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    let info = unsafe { PgBox::from_pg(info) };
    let mut stats = unsafe { PgBox::from_pg(stats) };
    let index_relation = unsafe { PgRelation::from_pg(info.index) };
    let callback =
        callback.expect("the ambulkdelete() callback should be a valid function pointer");
    let callback = move |ctid_val: u64| unsafe {
        let mut ctid = ItemPointerData::default();
        crate::postgres::utils::u64_to_item_pointer(ctid_val, &mut ctid);
        callback(&mut ctid, callback_state)
    };

    let _merge_lock = unsafe { MergeLock::acquire_for_delete(index_relation.oid()) };
    let mut writer = SearchIndexWriter::open(
        &index_relation,
        BlockDirectoryType::BulkDelete,
        WriterResources::Vacuum,
    )
    .expect("ambulkdelete: should be able to open a SearchIndexWriter");
    let reader = SearchIndexReader::open(&index_relation, BlockDirectoryType::BulkDelete, false)
        .expect("ambulkdelete: should be able to open a SearchIndexReader");

    let ctid_field = writer.get_ctid_field();
    let mut did_delete = false;

    for segment_reader in reader.searcher().segment_readers() {
        let inverted_index = segment_reader
            .inverted_index(ctid_field)
            .expect("tantivy inverted index should contain the ctid field");
        let termdict = inverted_index.terms();
        let mut allterms = termdict
            .stream()
            .expect("should be able to stream all terms from the inverted index");
        while let Some((term_bytes, term_info)) = allterms.next() {
            let mut postings = inverted_index
                .read_postings_from_terminfo(term_info, IndexRecordOption::Basic)
                .expect("should be able to retrieve postings for TermInfo");
            loop {
                let ctid = postings.ctid_value();
                let ctid = ((ctid.0 as u64) << 16) | ctid.1 as u64;
                if callback(ctid) {
                    did_delete = true;
                    writer
                        .delete_term(Term::from_field_bytes(ctid_field, term_bytes))
                        .expect("ambulkdelete: deleting ctid Term should succeed");
                }
                if postings.advance() == tantivy::TERMINATED {
                    break;
                }
            }
        }
    }

    // Don't merge here, amvacuumcleanup will merge
    writer
        .commit(false)
        .expect("ambulkdelete: commit should succeed");

    if stats.is_null() {
        stats = unsafe {
            PgBox::from_pg(
                pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBulkDeleteResult>()).cast(),
            )
        };
        stats.pages_deleted = 0;
    }

    // As soon as ambulkdelete returns, Postgres will update the visibility map
    // This can cause concurrent scans that have just read ctids, which are dead but
    // are about to be marked visible, to return wrong results. To guard against this,
    // we acquire a cleanup lock that guarantees that there are no pins on the index,
    // which means that all concurrent scans have completed. This lock is then released in
    // amvacuumcleanup, at which point the visibility map is updated and concurrent scans
    // are safe to resume.
    if did_delete {
        unsafe {
            let cleanup_buffer = pg_sys::ReadBufferExtended(
                info.index,
                pg_sys::ForkNumber::MAIN_FORKNUM,
                CLEANUP_LOCK,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                info.strategy,
            );
            pg_sys::LockBufferForCleanup(cleanup_buffer);

            let mut opaque = PgBox::<BulkDeleteData>::alloc0();
            opaque.stats = *stats;
            opaque.cleanup_lock = cleanup_buffer;
            opaque.into_pg() as *mut pg_sys::IndexBulkDeleteResult
        }
    } else {
        stats.into_pg()
    }
}
