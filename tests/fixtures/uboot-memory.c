// Native pinned U-Boot functions and libfdt are linked with the production Rust MC policy.
int main(int argc, char **argv)
{
    assert(argc == 3);
    FILE *file = fopen(argv[2], "rb");
    assert(file && fread(mc_registers, 1, sizeof(mc_registers), file) == sizeof(mc_registers));
    fclose(file);
    // Include a real-style firmware carveout in the high RAM bank.
    mc_registers[0x9a0 / 4] = 0x7f000000;
    mc_registers[0x9a8 / 4] = 1;
    mc_registers[0x9a4 / 4] = 16;
    u32 original[1024];
    memcpy(original, mc_registers, sizeof(original));
    assert(sv_mc_valid(mc_registers));
    data.bd = &bd;
    data.ram_size = query_sdram_size();
    assert(data.ram_size == 0x100000000ULL);
    assert(board_get_usable_ram_top(data.ram_size) == 0xffd00000ULL);
    assert(!dram_init_banksize());
    assert(bd.bi_dram[0].size == 0x7fd00000ULL);
    assert(bd.bi_dram[1].start == 0x100000000ULL && bd.bi_dram[1].size == 0x7f000000ULL);

    virtualized = true;
    data.ram_size = query_sdram_size();
    assert(board_get_usable_ram_top(data.ram_size) == 0xfec00000ULL);
    assert(!dram_init_banksize());
    assert(bd.bi_dram[0].start == 0x80000000ULL && bd.bi_dram[0].size == 0x7ec00000ULL);
    assert(bd.bi_dram[1].start == 0x100000000ULL && bd.bi_dram[1].size == 0x7f000000ULL);
    assert(bd.bi_dram[2].size == 0);
    assert(data.pci_ram_top == 0xfec00000ULL);
    mc_registers[0x50 / 4] = 0x80001800;
    assert(sv_mc_valid(mc_registers) && query_sdram_size() == data.ram_size);
    mc_registers[0x50 / 4] = original[0x50 / 4];

    unsigned char *input = malloc(1 << 20), *os = malloc(1 << 20);
    assert(input && os);
    file = fopen(argv[1], "rb");
    assert(file);
    size_t length = fread(input, 1, 1 << 20, file);
    fclose(file);
    assert(length >= 40 && !fdt_check_header(input));
    assert(!fdt_open_into(input, os, 1 << 20));
    int chosen = fdt_path_offset(os, "/chosen");
    assert(chosen >= 0);
    assert(!fdt_setprop_string(os, chosen, "bootargs", "maxcpus=1"));
    assert(!fdt_setprop_u64(os, chosen, "linux,initrd-start", 0x92000040));
    assert(!fdt_add_mem_rsv(os, 0xf5a00000, 0x400000));
    int reservations = fdt_num_mem_rsv(os);
    assert(!arch_fixup_fdt(os));
    int memory = fdt_path_offset(os, "/memory"), len;
    const fdt64_t *reg = fdt_getprop(os, memory, "reg", &len);
    assert(reg && len == 48);
    for (int i = 0; i < 3; i++) {
        assert(fdt64_to_cpu(reg[i*2]) == bd.bi_dram[i].start);
        assert(fdt64_to_cpu(reg[i*2+1]) == bd.bi_dram[i].size);
    }
    int nodes = 0, node = -1;
    while ((node = fdt_node_offset_by_prop_value(os, node, "device_type", "memory", 7)) >= 0) nodes++;
    assert(nodes == 1);
    assert(fdt_num_mem_rsv(os) == reservations);
    chosen = fdt_path_offset(os, "/chosen");
    const char *args = fdt_getprop(os, chosen, "bootargs", &len);
    assert(args && !strcmp(args, "maxcpus=1"));
    reg = fdt_getprop(os, chosen, "linux,initrd-start", &len);
    assert(reg && len == 8 && fdt64_to_cpu(*reg) == 0x92000040);
    int ramoops = fdt_path_offset(os, "/reserved-memory/ramoops_carveout");
    reg = fdt_getprop(os, ramoops, "reg", &len);
    assert(reg && len == 16 && fdt64_to_cpu(reg[0]) == 0xb0000000ULL && fdt64_to_cpu(reg[1]) == 0x200000);
    assert(!memcmp(mc_registers, original, sizeof(original)));
    free(input);
    free(os);
    return 0;
}
