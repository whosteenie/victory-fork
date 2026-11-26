@{
    Hash = @{
        Description = 'Production card hash and compare timings, including adjacent-range scheduling'
        Package = 'victory-gpu'
        Filter = 'host::card_replay::replay_card_hashes'
        Ignored = $true
        Backend = 'hardware'
        SampleKind = 'sample'
        ConfigurationKinds = @('init','mode')
        Modes = @('scalar','grouped','grouped_joined')
        Metrics = @('prepare_us','record_us','submit_us','wait_us','upload_hash_us','compare_us')
        SumMetrics = @{ gpu_total_us = @('upload_hash_us','compare_us') }
    }
    Parity = @{
        Description = 'Exact production hash words and flags against the CPU oracle'
        Package = 'victory-gpu'
        Filter = 'host::card_replay::card_hash_parity'
        Ignored = $true
        Backend = 'selectable'
        Debug = $true
    }
    Indirect = @{
        Description = 'Counted ExecuteIndirect, record stride, preserved source, and shared clamp budget'
        Package = 'victory-gpu'
        Filter = 'indirect::tests::warp_count_buffer_ignores_garbage_tail_and_preserves_source'
        Backend = 'warp'
        RequiredCheck = 'counted_indirect'
    }
    Mapping = @{
        Description = 'XMem map, remap, aliases, holes, lifetime and mapping latency'
        Package = 'victory-hle'
        Filter = 'modules::xmem::replay::replay_page_mappings'
        Ignored = $true
        Backend = 'cpu'
        SampleKind = 'mapping_sample'
        ConfigurationKinds = @('mapping_init')
        Modes = @('map_run','overlap_remap','alias_repeat','unmap_middle','remap_hole','allocate_map_free','unmap_batch')
        Metrics = @('wall_us')
    }
}
