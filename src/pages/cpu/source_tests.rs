// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 诊断来源测试
//
//   文件:       src/pages/cpu/source_tests.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! CPU 原生拓扑、PDH 映射和 WMI 类型转换的单元与本机集成测试。

#[cfg(test)]
mod tests {
    use std::mem::size_of;
    use std::slice;

    use windows_sys::Win32::System::SystemInformation::{
        CACHE_RELATIONSHIP, CacheUnified, GROUP_AFFINITY, GROUP_RELATIONSHIP,
        NUMA_NODE_RELATIONSHIP, PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_RELATIONSHIP,
        RelationCache, RelationGroup, RelationNumaNodeEx, RelationProcessorCore,
        RelationProcessorPackage, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    use crate::pages::cpu::firmware::{CpuFirmwareCollector, wmi_i4_to_u16, wmi_i4_to_u32};
    use crate::pages::cpu::model::{
        CpuArchitecture, CpuComponentUpdate, CpuDetailRefresh, CpuDetailRequest, CpuTopologyKey,
    };
    use crate::pages::cpu::native::{CpuNativeCollector, parse_relation_all};
    use crate::pages::cpu::pdh::{
        PdhArrayValue, PdhProcessorInstance, effective_frequency_mhz, parse_processor_instance,
        validate_frequencies,
    };
    use crate::system::cpu_topology::{CoreClass, LogicalProcessorId, ProcessorTopologyIdentity};

    fn id(group: u16, number: u8) -> LogicalProcessorId {
        LogicalProcessorId { group, number }
    }

    fn topology_key() -> CpuTopologyKey {
        CpuTopologyKey(vec![
            ProcessorTopologyIdentity {
                id: id(0, 0),
                physical_core_index: 0,
                smt_index: Some(0),
                class: CoreClass::Uniform,
            },
            ProcessorTopologyIdentity {
                id: id(0, 1),
                physical_core_index: 0,
                smt_index: Some(1),
                class: CoreClass::Uniform,
            },
        ])
    }

    fn affinity(mask: usize) -> GROUP_AFFINITY {
        GROUP_AFFINITY {
            Mask: mask,
            Group: 0,
            Reserved: [0; 3],
        }
    }

    fn processor_record(relationship: i32, mask: usize) -> Vec<u8> {
        let mut processor = PROCESSOR_RELATIONSHIP {
            GroupCount: 1,
            ..Default::default()
        };
        processor.GroupMask[0] = affinity(mask);
        let mut record = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX {
            Relationship: relationship,
            Size: size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() as u32,
            ..Default::default()
        };
        record.Anonymous.Processor = processor;
        record_bytes(&record)
    }

    fn numa_record(mask: usize) -> Vec<u8> {
        let mut numa = NUMA_NODE_RELATIONSHIP {
            NodeNumber: 0,
            GroupCount: 1,
            ..Default::default()
        };
        numa.Anonymous.GroupMask = affinity(mask);
        let mut record = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX {
            Relationship: RelationNumaNodeEx,
            Size: size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() as u32,
            ..Default::default()
        };
        record.Anonymous.NumaNode = numa;
        record_bytes(&record)
    }

    fn group_record(mask: usize) -> Vec<u8> {
        let mut group = GROUP_RELATIONSHIP {
            MaximumGroupCount: 1,
            ActiveGroupCount: 1,
            ..Default::default()
        };
        group.GroupInfo[0].MaximumProcessorCount = usize::BITS as u8;
        group.GroupInfo[0].ActiveProcessorCount = mask.count_ones() as u8;
        group.GroupInfo[0].ActiveProcessorMask = mask;
        let mut record = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX {
            Relationship: RelationGroup,
            Size: size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() as u32,
            ..Default::default()
        };
        record.Anonymous.Group = group;
        record_bytes(&record)
    }

    fn cache_record(mask: usize) -> Vec<u8> {
        let mut cache = CACHE_RELATIONSHIP {
            Level: 2,
            Associativity: 8,
            LineSize: 64,
            CacheSize: 1_048_576,
            Type: CacheUnified,
            GroupCount: 1,
            ..Default::default()
        };
        cache.Anonymous.GroupMask = affinity(mask);
        let mut record = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX {
            Relationship: RelationCache,
            Size: size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() as u32,
            ..Default::default()
        };
        record.Anonymous.Cache = cache;
        record_bytes(&record)
    }

    fn record_bytes(record: &SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) -> Vec<u8> {
        unsafe {
            slice::from_raw_parts(
                (record as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX).cast::<u8>(),
                size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            )
            .to_vec()
        }
    }

    fn valid_relation_all() -> Vec<u8> {
        [
            processor_record(RelationProcessorCore, 0b11),
            processor_record(RelationProcessorPackage, 0b11),
            numa_record(0b11),
            group_record(0b11),
            cache_record(0b11),
        ]
        .concat()
    }

    #[test]
    fn processor_frequency_instances_follow_the_documented_numa_format() {
        assert_eq!(
            parse_processor_instance("0,31").unwrap(),
            Some(PdhProcessorInstance {
                numa_node: 0,
                numa_index: 31
            })
        );
        assert_eq!(
            parse_processor_instance("2,7").unwrap(),
            Some(PdhProcessorInstance {
                numa_node: 2,
                numa_index: 7
            })
        );
        assert_eq!(parse_processor_instance("_Total").unwrap(), None);
        assert_eq!(parse_processor_instance("0,_total").unwrap(), None);
        assert!(parse_processor_instance("31").is_err());
        assert!(parse_processor_instance("0,1,2").is_err());
        assert!(parse_processor_instance("-1,0").is_err());
        assert!(parse_processor_instance("group,_Total").is_err());
    }

    #[test]
    fn frequency_set_requires_exact_unique_processor_membership() {
        let expected = [id(0, 0), id(0, 1)];
        let nominal = [
            PdhArrayValue {
                instance: "0,0".into(),
                value: 3_000,
            },
            PdhArrayValue {
                instance: "0,1".into(),
                value: 3_200,
            },
            PdhArrayValue {
                instance: "_Total".into(),
                value: 3_100,
            },
        ];
        let performance = [
            PdhArrayValue {
                instance: "0,0".into(),
                value: 150.0,
            },
            PdhArrayValue {
                instance: "0,1".into(),
                value: 50.0,
            },
            PdhArrayValue {
                instance: "_Total".into(),
                value: 100.0,
            },
        ];
        assert_eq!(
            validate_frequencies(&nominal, &performance, &expected).unwrap(),
            (3_050, 1_600, 4_500)
        );

        let duplicate = [nominal[0].clone(), nominal[0].clone()];
        assert!(validate_frequencies(&duplicate, &performance, &expected).is_err());
        assert!(validate_frequencies(&nominal[..1], &performance, &expected).is_err());
        assert!(validate_frequencies(&nominal, &performance[..1], &expected).is_err());

        let mut mismatched = performance.clone();
        mismatched[1].instance = "1,0".into();
        assert!(validate_frequencies(&nominal, &mismatched, &expected).is_err());
    }

    #[test]
    fn effective_frequency_preserves_boost_and_rejects_invalid_performance() {
        assert_eq!(effective_frequency_mhz(2_401, 177.77).unwrap(), 4_268);
        assert!(effective_frequency_mhz(2_401, -1.0).is_err());
        assert!(effective_frequency_mhz(2_401, f64::NAN).is_err());
    }

    #[test]
    fn architecture_mapping_keeps_unknown_values_explicit() {
        assert_eq!(
            CpuArchitecture::from_windows(PROCESSOR_ARCHITECTURE_AMD64),
            CpuArchitecture::X64
        );
        assert_eq!(
            CpuArchitecture::from_windows(1234),
            CpuArchitecture::Unknown(1234)
        );
    }

    #[test]
    fn wmi_unsigned_integer_mapping_matches_automation_contract() {
        assert_eq!(wmi_i4_to_u16(0).unwrap(), 0);
        assert_eq!(wmi_i4_to_u16(i32::from(u16::MAX)).unwrap(), u16::MAX);
        assert!(wmi_i4_to_u16(-1).is_err());
        assert!(wmi_i4_to_u16(i32::from(u16::MAX) + 1).is_err());

        assert_eq!(wmi_i4_to_u32(0), 0);
        assert_eq!(wmi_i4_to_u32(i32::MAX), i32::MAX as u32);
        assert_eq!(wmi_i4_to_u32(i32::MIN), 0x8000_0000);
        assert_eq!(wmi_i4_to_u32(-1), u32::MAX);
    }

    #[test]
    fn relation_all_accepts_complete_group_aware_records() {
        let topology = parse_relation_all(&valid_relation_all(), &topology_key()).unwrap();
        assert_eq!(topology.package_count, 1);
        assert_eq!(topology.numa_node_count, 1);
        assert_eq!(topology.group_count, 1);
        assert_eq!(topology.caches.len(), 1);
        assert_eq!(topology.caches[0].instance_count, 1);
        assert_eq!(topology.caches[0].total_bytes, 1_048_576);
    }

    #[test]
    fn relation_all_rejects_truncation_and_invalid_record_sizes() {
        let mut truncated = valid_relation_all();
        truncated.pop();
        assert!(parse_relation_all(&truncated, &topology_key()).is_err());

        let mut invalid_size = valid_relation_all();
        invalid_size[4..8].copy_from_slice(&4u32.to_ne_bytes());
        assert!(parse_relation_all(&invalid_size, &topology_key()).is_err());
    }

    #[test]
    fn relation_all_rejects_duplicate_missing_and_out_of_range_members() {
        let mut duplicate = valid_relation_all();
        duplicate.extend(processor_record(RelationProcessorCore, 0b11));
        assert!(parse_relation_all(&duplicate, &topology_key()).is_err());

        let missing = [
            processor_record(RelationProcessorCore, 0b11),
            processor_record(RelationProcessorPackage, 0b01),
            numa_record(0b11),
            group_record(0b11),
        ]
        .concat();
        assert!(parse_relation_all(&missing, &topology_key()).is_err());

        let out_of_range = [
            processor_record(RelationProcessorCore, 0b111),
            processor_record(RelationProcessorPackage, 0b11),
            numa_record(0b11),
            group_record(0b11),
        ]
        .concat();
        assert!(parse_relation_all(&out_of_range, &topology_key()).is_err());
    }

    #[test]
    #[ignore = "requires live Windows topology, WMI, and PDH services"]
    fn live_cpu_detail_sources_return_one_coherent_topology() {
        let processor_count = unsafe {
            windows_sys::Win32::System::Threading::GetActiveProcessorCount(
                windows_sys::Win32::System::Threading::ALL_PROCESSOR_GROUPS,
            )
        };
        assert_ne!(processor_count, 0);
        assert_ne!(processor_count, u32::MAX);
        let topology =
            crate::system::cpu_topology::query_processor_topology(processor_count as usize);
        let key = CpuTopologyKey::from_topology(&topology)
            .expect("live processor topology should be complete");
        let mut native_collector = CpuNativeCollector::new();
        let native = native_collector.collect(CpuDetailRequest {
            topology_key: key.clone(),
            refresh: CpuDetailRefresh::Prewarm,
        });
        assert_eq!(native.topology_key, key);
        assert!(
            matches!(&native.topology, CpuComponentUpdate::Success(_)),
            "{:?}",
            native.topology
        );
        assert!(
            matches!(&native.features, CpuComponentUpdate::Success(_)),
            "{:?}",
            native.features
        );
        assert!(
            matches!(&native.dynamic, CpuComponentUpdate::Unchanged),
            "{:?}",
            native.dynamic
        );
        assert!(native.pdh_baseline_timestamp_ms.is_some());

        let mut firmware_collector = CpuFirmwareCollector::new();
        let firmware = firmware_collector.collect(CpuDetailRequest {
            topology_key: key.clone(),
            refresh: CpuDetailRefresh::Prewarm,
        });
        assert_eq!(firmware.topology_key, key);
        assert!(
            matches!(&firmware.firmware, CpuComponentUpdate::Success(_)),
            "{:?}",
            firmware.firmware
        );

        std::thread::sleep(std::time::Duration::from_secs(1));
        let native = native_collector.collect(CpuDetailRequest {
            topology_key: key,
            refresh: CpuDetailRefresh::Periodic,
        });
        assert!(
            matches!(&native.dynamic, CpuComponentUpdate::Success(_)),
            "{:?}",
            native.dynamic
        );
    }
}
