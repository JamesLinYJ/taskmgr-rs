// +-------------------------------------------------------------------------
//
//   taskmgr-rs - GPU 数据源测试
//
//   文件:       src/pages/gpu/source_tests.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Verifies strict counter parsing, aggregation, topology identity, and metadata validation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::counters::{
    EngineReading, GpuCollector, MemoryReading, assemble_samples, parse_engine_instance,
    parse_memory_instance, percentage_to_u8, validated_engine_kinds,
};
use super::inventory::{
    INSTALLED_MEMORY_VALUE_NAME, fixed_wide_value_name, validate_installed_memory_registry_value,
    validated_physical_adapter_count,
};
use super::metadata::{
    GpuMetadataCollector, device_instance_id_from_pnp_key, validated_hardware_reserved_memory,
};
use super::model::*;

fn adapter_info(id: GpuAdapterId) -> Arc<GpuAdapterInfo> {
    Arc::new(GpuAdapterInfo {
        id,
        enumeration_index: 0,
        name: "Test GPU".to_string(),
        vendor_id: 1,
        device_id: 2,
        subsystem_id: 3,
        revision: 4,
        dedicated_limit_bytes: Some(8 * 1024 * 1024 * 1024),
        shared_limit_bytes: Some(4 * 1024 * 1024 * 1024),
    })
}

#[test]
fn parses_engine_identity_without_guessing_fields() {
    let parsed = parse_engine_instance(
        "pid_104888_luid_0x00000001_0x000119b6_phys_2_eng_7_engtype_videoencode",
    )
    .unwrap();
    assert_eq!(parsed.pid, 104888);
    assert_eq!(parsed.id.adapter.luid.high_part, 1);
    assert_eq!(parsed.id.adapter.luid.low_part, 0x119b6);
    assert_eq!(parsed.id.adapter.physical_index, 2);
    assert_eq!(parsed.id.ordinal, 7);
    assert_eq!(parsed.kind, GpuEngineKind::VideoEncode);
}

#[test]
fn rejects_truncated_or_overflowing_counter_instances() {
    for value in [
        "pid_1_luid_0x0_0x1_phys_0_eng_0",
        "pid_4294967296_luid_0x0_0x1_phys_0_eng_0_engtype_3d",
        "pid_1_luid_0x000000000_0x1_phys_0_eng_0_engtype_3d",
        "pid_1_luid_1_0x1_phys_0_eng_0_engtype_3d",
    ] {
        assert!(parse_engine_instance(value).is_err(), "{value}");
    }
}

#[test]
fn parses_memory_identity() {
    let parsed = parse_memory_instance("luid_0xffffffff_0x00000002_phys_3").unwrap();
    assert_eq!(parsed.luid.high_part, -1);
    assert_eq!(parsed.luid.low_part, 2);
    assert_eq!(parsed.physical_index, 3);
}

#[test]
fn inventory_order_can_preserve_dxgi_enumeration_order() {
    let make_info = |low_part: u32, enumeration_index: u32| {
        let id = GpuAdapterId {
            luid: AdapterLuid::from_parts(0, low_part),
            physical_index: 0,
        };
        let mut info = (*adapter_info(id)).clone();
        info.enumeration_index = enumeration_index;
        Arc::new(info)
    };
    let mut infos = [make_info(1, 2), make_info(2, 0), make_info(3, 1)];
    infos.sort_by_key(|info| (info.enumeration_index, info.id.physical_index));
    assert_eq!(
        infos
            .iter()
            .map(|info| info.id.luid.low_part)
            .collect::<Vec<_>>(),
        vec![2, 3, 1]
    );
}

#[test]
fn physical_adapter_count_is_never_guessed() {
    assert_eq!(validated_physical_adapter_count(2), Ok(2));
    assert_eq!(
        validated_physical_adapter_count(0),
        Err(GpuSampleError::InvalidData {
            context: "D3DKMT physical adapter count"
        })
    );
}

#[test]
fn installed_memory_registry_value_requires_an_exact_qword() {
    assert_eq!(
        validate_installed_memory_registry_value(8, 8 * 1024 * 1024 * 1024),
        Ok(8 * 1024 * 1024 * 1024)
    );
    assert!(validate_installed_memory_registry_value(4, u32::MAX as u64).is_err());
    assert!(validate_installed_memory_registry_value(8, 0).is_err());
}

#[test]
fn hardware_reserved_memory_is_a_checked_difference() {
    let gib = 1024 * 1024 * 1024;
    assert_eq!(
        validated_hardware_reserved_memory(Some(8 * gib), Some(7 * gib)),
        Ok(Some(gib))
    );
    assert_eq!(
        validated_hardware_reserved_memory(None, Some(7 * gib)),
        Ok(None)
    );
    assert_eq!(
        validated_hardware_reserved_memory(Some(8 * gib), None),
        Ok(None)
    );
    assert!(validated_hardware_reserved_memory(Some(7 * gib), Some(8 * gib)).is_err());
}

#[test]
fn registry_value_name_is_nul_terminated_without_truncation() {
    let value = fixed_wide_value_name::<260>(INSTALLED_MEMORY_VALUE_NAME).unwrap();
    let expected = INSTALLED_MEMORY_VALUE_NAME
        .encode_utf16()
        .collect::<Vec<_>>();
    assert_eq!(&value[..expected.len()], expected.as_slice());
    assert_eq!(value[expected.len()], 0);
    assert!(fixed_wide_value_name::<4>("four").is_err());
}

#[test]
fn aggregates_process_instances_per_engine_and_uses_busiest_engine() {
    let id = GpuAdapterId {
        luid: AdapterLuid::from_parts(0, 0x1234),
        physical_index: 0,
    };
    let info = adapter_info(id);
    let known = HashSet::from([id.luid]);
    let engines = vec![
        EngineReading {
            instance_name: "pid_10_luid_0x0_0x1234_phys_0_eng_1_engtype_3d".to_string(),
            utilization: 35.4,
        },
        EngineReading {
            instance_name: "pid_11_luid_0x0_0x1234_phys_0_eng_1_engtype_3d".to_string(),
            utilization: 30.4,
        },
        EngineReading {
            instance_name: "pid_10_luid_0x0_0x1234_phys_0_eng_2_engtype_copy".to_string(),
            utilization: 80.2,
        },
    ];
    let result = assemble_samples(
        &[info],
        &known,
        engines,
        vec![MemoryReading {
            instance_name: "luid_0x0_0x1234_phys_0".to_string(),
            bytes: 1024,
        }],
        vec![MemoryReading {
            instance_name: "luid_0x0_0x1234_phys_0".to_string(),
            bytes: 2048,
        }],
        HashMap::from([(id, Ok(Some(0)))]),
    )
    .unwrap();
    assert_eq!(result[0].engines[0].utilization_percent, 66);
    assert_eq!(result[0].engines[1].utilization_percent, 80);
    assert_eq!(result[0].overall_utilization_percent, 80);
    assert_eq!(result[0].dedicated_usage_bytes, 1024);
    assert_eq!(result[0].shared_usage_bytes, 2048);
    assert_eq!(result[0].temperature_deci_c, Some(0));

    let known_kinds = validated_engine_kinds(&HashMap::new(), &result).unwrap();
    let mut changed = result.clone();
    changed[0].engines[0].kind = GpuEngineKind::Copy;
    assert_eq!(
        validated_engine_kinds(&known_kinds, &changed).unwrap_err(),
        GpuSampleError::InvalidData {
            context: "GPU engine type changed without a topology generation"
        }
    );
}

#[test]
fn missing_memory_counter_instances_are_not_reported_as_zero() {
    let id = GpuAdapterId {
        luid: AdapterLuid::from_parts(0, 1),
        physical_index: 0,
    };
    let error = assemble_samples(
        &[adapter_info(id)],
        &HashSet::from([id.luid]),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::from([(id, Ok(None))]),
    )
    .unwrap_err();
    assert_eq!(
        error,
        GpuSampleError::InvalidData {
            context: "missing dedicated GPU memory instance"
        }
    );
}

#[test]
fn physical_indices_under_one_luid_remain_distinct() {
    let luid = AdapterLuid::from_parts(0, 0x44);
    let first = GpuAdapterId {
        luid,
        physical_index: 0,
    };
    let second = GpuAdapterId {
        luid,
        physical_index: 1,
    };
    let samples = assemble_samples(
        &[adapter_info(first), adapter_info(second)],
        &HashSet::from([luid]),
        vec![
            EngineReading {
                instance_name: "pid_1_luid_0x0_0x44_phys_0_eng_0_engtype_3d".to_string(),
                utilization: 10.0,
            },
            EngineReading {
                instance_name: "pid_2_luid_0x0_0x44_phys_1_eng_0_engtype_3d".to_string(),
                utilization: 20.0,
            },
        ],
        vec![
            MemoryReading {
                instance_name: "luid_0x0_0x44_phys_0".to_string(),
                bytes: 100,
            },
            MemoryReading {
                instance_name: "luid_0x0_0x44_phys_1".to_string(),
                bytes: 200,
            },
        ],
        vec![
            MemoryReading {
                instance_name: "luid_0x0_0x44_phys_0".to_string(),
                bytes: 300,
            },
            MemoryReading {
                instance_name: "luid_0x0_0x44_phys_1".to_string(),
                bytes: 400,
            },
        ],
        HashMap::from([(first, Ok(None)), (second, Ok(None))]),
    )
    .unwrap();

    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].info.id, first);
    assert_eq!(samples[0].overall_utilization_percent, 10);
    assert_eq!(samples[0].dedicated_usage_bytes, 100);
    assert_eq!(samples[0].shared_usage_bytes, 300);
    assert_eq!(samples[1].info.id, second);
    assert_eq!(samples[1].overall_utilization_percent, 20);
    assert_eq!(samples[1].dedicated_usage_bytes, 200);
    assert_eq!(samples[1].shared_usage_bytes, 400);
}

#[test]
fn known_non_displayed_adapter_instances_are_ignored_without_weakening_identity_checks() {
    let displayed = GpuAdapterId {
        luid: AdapterLuid::from_parts(0, 0x10),
        physical_index: 0,
    };
    let non_displayed_luid = AdapterLuid::from_parts(0, 0x20);
    let samples = assemble_samples(
        &[adapter_info(displayed)],
        &HashSet::from([displayed.luid, non_displayed_luid]),
        vec![EngineReading {
            instance_name: "pid_1_luid_0x0_0x20_phys_0_eng_0_engtype_3d".to_string(),
            utilization: 50.0,
        }],
        vec![MemoryReading {
            instance_name: "luid_0x0_0x10_phys_0".to_string(),
            bytes: 100,
        }],
        vec![MemoryReading {
            instance_name: "luid_0x0_0x10_phys_0".to_string(),
            bytes: 200,
        }],
        HashMap::from([(displayed, Ok(None))]),
    )
    .unwrap();
    assert!(samples[0].engines.is_empty());

    let unknown = EngineReading {
        instance_name: "pid_1_luid_0x0_0x30_phys_0_eng_0_engtype_3d".to_string(),
        utilization: 1.0,
    };
    assert!(
        assemble_samples(
            &[adapter_info(displayed)],
            &HashSet::from([displayed.luid, non_displayed_luid]),
            vec![unknown],
            Vec::new(),
            Vec::new(),
            HashMap::from([(displayed, Ok(None))]),
        )
        .is_err()
    );
}

#[test]
fn clamps_only_the_final_engine_display_value() {
    assert_eq!(percentage_to_u8(101.7), 100);
    assert_eq!(percentage_to_u8(49.5), 50);
}

#[test]
fn rejects_duplicate_process_engine_instances() {
    let id = GpuAdapterId {
        luid: AdapterLuid::from_parts(0, 7),
        physical_index: 0,
    };
    let info = adapter_info(id);
    let reading = EngineReading {
        instance_name: "pid_1_luid_0x0_0x7_phys_0_eng_0_engtype_3d".to_string(),
        utilization: 1.0,
    };
    assert!(
        assemble_samples(
            &[info],
            &HashSet::from([id.luid]),
            vec![reading.clone(), reading],
            Vec::new(),
            Vec::new(),
            HashMap::new(),
        )
        .is_err()
    );
}

#[test]
fn extracts_setupapi_instance_id_from_registry_pnp_key() {
    assert_eq!(
            device_instance_id_from_pnp_key(
                r"\Registry\Machine\System\CurrentControlSet\Enum\PCI\VEN_1234&DEV_5678\ABC\Device Parameters"
            )
            .as_deref(),
            Some(r"PCI\VEN_1234&DEV_5678\ABC")
        );
}

#[test]
#[ignore = "requires live Windows DXGI, KMT, SetupAPI, D3D12, and PDH services"]
fn live_gpu_sources_submit_inventory_before_optional_details() {
    let mut collector = GpuCollector::new();
    let inventory = match collector.collect().expect("GPU inventory query") {
        GpuCollectOutcome::Inventory(inventory) => inventory,
        other => panic!("first GPU completion was not inventory: {other:?}"),
    };
    assert_ne!(inventory.generation, 0);

    let mut metadata_collector = GpuMetadataCollector::new();
    let metadata = metadata_collector
        .collect(GpuMetadataRequest {
            generation: inventory.generation,
            adapters: inventory.adapters.clone(),
        })
        .expect("GPU metadata query");
    assert_eq!(metadata.generation, inventory.generation);
    assert_eq!(metadata.adapters.len(), inventory.adapters.len());

    match collector.collect().expect("second GPU sample") {
        GpuCollectOutcome::AwaitingBaseline { generation } => {
            assert_eq!(generation, inventory.generation);
        }
        GpuCollectOutcome::Dynamic(snapshot) => {
            assert_eq!(snapshot.generation, inventory.generation);
            assert_eq!(snapshot.adapters.len(), inventory.adapters.len());
        }
        GpuCollectOutcome::Inventory(_) => panic!("inventory was submitted twice"),
    }
}
