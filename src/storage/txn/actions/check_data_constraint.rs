use crate::storage::mvcc::{ErrorInner, MvccTxn, Result as MvccResult};
use crate::storage::Snapshot;
use txn_types::{Key, TimeStamp, Write, WriteType};

/// Checks the existence of the key according to `should_not_exist`.
/// If not, returns an `AlreadyExist` error.
pub(crate) fn check_data_constraint<S: Snapshot>(
    txn: &mut MvccTxn<S>,
    should_not_exist: bool,
    write: &Write,
    write_commit_ts: TimeStamp,
    key: &Key,
) -> MvccResult<()> {
    if !should_not_exist || write.write_type == WriteType::Delete {
        return Ok(());
    }

    // The current key exists under any of the following conditions:
    // 1.The current write type is `PUT`
    // 2.The current write type is `Rollback` or `Lock`, and the key have an older version.
    if write.write_type == WriteType::Put || txn.key_exist(&key, write_commit_ts.prev())? {
        return Err(ErrorInner::AlreadyExist { key: key.to_raw()? }.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::storage::mvcc::tests::write;
    use crate::storage::mvcc::{ErrorInner, MvccTxn, Result as MvccResult};
    use crate::storage::txn::actions::check_data_constraint::check_data_constraint;
    use crate::storage::{Engine, TestEngineBuilder};
    use concurrency_manager::ConcurrencyManager;
    use kvproto::kvrpcpb::Context;
    use txn_types::{Key, TimeStamp, Write, WriteType};

    #[test]
    fn test_check_data_constraint() {
        let engine = TestEngineBuilder::new().build().unwrap();
        let cm = ConcurrencyManager::new(42.into());
        let snapshot = engine.snapshot(Default::default()).unwrap();
        let mut txn = MvccTxn::new(snapshot, TimeStamp::new(2), true, cm.clone());
        txn.put_write(
            Key::from_raw(b"a"),
            TimeStamp::new(5),
            Write::new(WriteType::Put, TimeStamp::new(2), None)
                .as_ref()
                .to_bytes(),
        );
        write(&engine, &Context::default(), txn.into_modifies());
        let snapshot = engine.snapshot(Default::default()).unwrap();
        let mut txn = MvccTxn::new(snapshot, TimeStamp::new(3), true, cm);

        struct Case {
            expected: MvccResult<()>,

            should_not_exist: bool,
            write: Write,
            write_commit_ts: TimeStamp,
            key: Key,
        }

        let cases = vec![
            // todo: add more cases
            Case {
                // should skip the check when `should_not_exist` is `false`
                expected: Ok(()),
                should_not_exist: false,
                write: Write::new(WriteType::Put, TimeStamp::new(3), None),
                write_commit_ts: Default::default(),
                key: Key::from_raw(b"a"),
            },
            Case {
                // should skip the check when `write_type` is `delete`
                expected: Ok(()),
                should_not_exist: true,
                write: Write::new(WriteType::Delete, TimeStamp::new(3), None),
                write_commit_ts: Default::default(),
                key: Key::from_raw(b"a"),
            },
            Case {
                // should detect conflict `Put`
                expected: Err(ErrorInner::AlreadyExist { key: b"a".to_vec() }.into()),
                should_not_exist: true,
                write: Write::new(WriteType::Put, TimeStamp::new(3), None),
                write_commit_ts: Default::default(),
                key: Key::from_raw(b"a"),
            },
            Case {
                // should detect an older version when the current write type is `Rollback`
                expected: Err(ErrorInner::AlreadyExist { key: b"a".to_vec() }.into()),
                should_not_exist: true,
                write: Write::new(WriteType::Rollback, TimeStamp::new(3), None),
                write_commit_ts: TimeStamp::new(6),
                key: Key::from_raw(b"a"),
            },
            Case {
                // should detect an older version when the current write type is `Lock`
                expected: Err(ErrorInner::AlreadyExist { key: b"a".to_vec() }.into()),
                should_not_exist: true,
                write: Write::new(WriteType::Lock, TimeStamp::new(10), None),
                write_commit_ts: TimeStamp::new(15),
                key: Key::from_raw(b"a"),
            },
        ];

        for Case {
            expected,
            should_not_exist,
            write,
            write_commit_ts,
            key,
        } in cases
        {
            let result =
                check_data_constraint(&mut txn, should_not_exist, &write, write_commit_ts, &key);
            assert_eq!(format!("{:?}", expected), format!("{:?}", result));
        }
    }
}
