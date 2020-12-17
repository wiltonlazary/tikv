// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use txn_types::{Key, Lock, TimeStamp, WriteType};

use crate::storage::{
    mvcc::txn::MissingLockAction,
    mvcc::{
        metrics::MVCC_CHECK_TXN_STATUS_COUNTER_VEC, ErrorInner, LockType, MvccTxn, ReleasedLock,
        Result, TxnCommitRecord,
    },
    Snapshot, TxnStatus,
};

pub fn check_txn_status_lock_exists<S: Snapshot>(
    txn: &mut MvccTxn<S>,
    primary_key: Key,
    mut lock: Lock,
    current_ts: TimeStamp,
    caller_start_ts: TimeStamp,
    force_sync_commit: bool,
    resolving_pessimistic_lock: bool,
) -> Result<(TxnStatus, Option<ReleasedLock>)> {
    // Never rollback or push forward min_commit_ts in check_txn_status if it's using async commit.
    // Rollback of async-commit locks are done during ResolveLock.
    if lock.use_async_commit {
        if force_sync_commit {
            info!(
                "fallback is set, check_txn_status treats it as a non-async-commit txn";
                "start_ts" => txn.start_ts,
                "primary_key" => ?primary_key,
            );
        } else {
            return Ok((TxnStatus::uncommitted(lock, false), None));
        }
    }

    let is_pessimistic_txn = !lock.for_update_ts.is_zero();
    if lock.ts.physical() + lock.ttl < current_ts.physical() {
        // If the lock is expired, clean it up.
        // If the resolving and primary key lock are both pessimistic locks, just unlock the
        // primary pessimistic lock and do not write rollback records.
        return if resolving_pessimistic_lock && lock.lock_type == LockType::Pessimistic {
            let released = txn.unlock_key(primary_key, is_pessimistic_txn);
            MVCC_CHECK_TXN_STATUS_COUNTER_VEC.pessimistic_rollback.inc();
            Ok((TxnStatus::PessimisticRollBack, released))
        } else {
            let released =
                txn.check_write_and_rollback_lock(primary_key, &lock, is_pessimistic_txn)?;
            MVCC_CHECK_TXN_STATUS_COUNTER_VEC.rollback.inc();
            Ok((TxnStatus::TtlExpire, released))
        };
    }

    // Although we won't really push forward min_commit_ts when caller_start_ts is max,
    // we should return MinCommitTsPushed result to the client to keep backward
    // compatibility.
    let mut min_commit_ts_pushed = caller_start_ts.is_max();

    // If lock.min_commit_ts is 0, it's not a large transaction and we can't push forward
    // its min_commit_ts otherwise the transaction can't be committed by old version TiDB
    // during rolling update.
    if !lock.min_commit_ts.is_zero()
        // If the caller_start_ts is max, it's a point get in the autocommit transaction.
        // We don't push forward lock's min_commit_ts and the point get can ignore the lock
        // next time because it's not committed.
        && !caller_start_ts.is_max()
        // Push forward the min_commit_ts so that reading won't be blocked by locks.
        && caller_start_ts >= lock.min_commit_ts
    {
        lock.min_commit_ts = caller_start_ts.next();

        if lock.min_commit_ts < current_ts {
            lock.min_commit_ts = current_ts;
        }

        txn.put_lock(primary_key, &lock);
        min_commit_ts_pushed = true;
        MVCC_CHECK_TXN_STATUS_COUNTER_VEC.update_ts.inc();
    }

    Ok((TxnStatus::uncommitted(lock, min_commit_ts_pushed), None))
}

pub fn check_txn_status_missing_lock<S: Snapshot>(
    txn: &mut MvccTxn<S>,
    primary_key: Key,
    mismatch_lock: Option<Lock>,
    action: MissingLockAction,
    resolving_pessimistic_lock: bool,
) -> Result<TxnStatus> {
    MVCC_CHECK_TXN_STATUS_COUNTER_VEC.get_commit_info.inc();

    match txn
        .reader
        .get_txn_commit_record(&primary_key, txn.start_ts)?
    {
        TxnCommitRecord::SingleRecord { commit_ts, write } => {
            if write.write_type == WriteType::Rollback {
                Ok(TxnStatus::RolledBack)
            } else {
                Ok(TxnStatus::committed(commit_ts))
            }
        }
        TxnCommitRecord::OverlappedRollback { .. } => Ok(TxnStatus::RolledBack),
        TxnCommitRecord::None { overlapped_write } => {
            if MissingLockAction::ReturnError == action {
                return Err(ErrorInner::TxnNotFound {
                    start_ts: txn.start_ts,
                    key: primary_key.into_raw()?,
                }
                .into());
            }
            if resolving_pessimistic_lock {
                return Ok(TxnStatus::LockNotExistDoNothing);
            }

            let ts = txn.start_ts;

            // collapse previous rollback if exist.
            if txn.collapse_rollback {
                txn.collapse_prev_rollback(primary_key.clone())?;
            }

            if let (Some(l), None) = (mismatch_lock, overlapped_write.as_ref()) {
                txn.mark_rollback_on_mismatching_lock(
                    &primary_key,
                    l,
                    action == MissingLockAction::ProtectedRollback,
                );
            }

            // Insert a Rollback to Write CF in case that a stale prewrite
            // command is received after a cleanup command.
            if let Some(write) = action.construct_write(ts, overlapped_write) {
                txn.put_write(primary_key, ts, write.as_ref().to_bytes());
            }
            MVCC_CHECK_TXN_STATUS_COUNTER_VEC.rollback.inc();

            Ok(TxnStatus::LockNotExist)
        }
    }
}
