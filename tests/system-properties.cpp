#include "systemproperties.h"
#include <QCoreApplication>
#include <cstdio>
#include <cerrno>
#include <QtEndian>

int main(int argc, char **argv)
{
    QCoreApplication app(argc, argv);
    int failures = 0;
    const auto check = [&](bool ok, const char *message) {
        if (!ok) { std::fprintf(stderr, "%s\n", message); ++failures; }
    };
    check(parseOsName("PRETTY_NAME=\"Example Linux 2\"\n").value == "Example Linux 2", "Double quoted OS caption");
    check(parseOsName("PRETTY_NAME='Example $literal'\n").value == "Example $literal", "Single quotes preserve literal dollars");
    check(parseOsName("PRETTY_NAME=\"Escaped \\\"quote\\\" and \\$dollar\"\n").value == "Escaped \"quote\" and $dollar",
          "Decode permitted shell escapes without evaluation");
    check(parseOsName("PRETTY_NAME=\"$(must-not-run)\"\n").value == "$(must-not-run)", "Command substitution remains literal text");
    check(parseOsName("PRETTY_NAME=\"unterminated\n").state == ReadState::Error, "Malformed OS quote is explicit error");
    check(parseOsName("NAME=Example\n").value == "Linux", "Missing PRETTY_NAME uses documented Linux default");
    check(parseOsName("PRETTY_NAME=First\nPRETTY_NAME=Second\n").value == "Second", "Last OS definition wins");
    QSet<int> cpus;
    check(parseCpuList("0-3,8,10-11", cpus) && cpus.size() == 7 && cpus.contains(11) && !cpus.contains(9),
          "Sparse CPU ranges retain actual IDs");
    const QSet<int> previous = cpus;
    check(!parseCpuList("3-1", cpus) && cpus == previous, "Reversed ranges fail without partial output");
    check(!parseCpuList("0-500000", cpus), "CPU list expansion is bounded");
    check(!parseCpuList("0,", cpus) && !parseCpuList("-1", cpus) && !parseCpuList("word", cpus),
          "Invalid CPU identifiers never become CPU zero");
    check(parseCpuInstructionSets("processor : 0\nflags : sse avx2 sse\n\nprocessor : 1\nflags : avx2 sse\n").value
          == "0, 1\tavx2 sse", "Identical CPU feature lists group without duplicate flags");
    const Attribute heterogeneous = parseCpuInstructionSets("processor : 0\nflags : sse avx2\n\nprocessor : 1\nflags : sse\n");
    check(heterogeneous.value.contains("0\tavx2 sse") && heterogeneous.value.contains("1\tsse"),
          "Different CPU flags never become a universal feature union");
    check(parseCpuInstructionSets("processor : 0\nFeatures : aes asimd\n").value == "0\taes asimd",
          "ARM feature field is retained");
    check(parseCpuInstructionSets("processor : 0\nisa : rv64imafdc\n").value == "0\trv64imafdc",
          "RISC-V ISA declaration is retained");
    check(parseCpuInstructionSets("processor : 0\nflags : sse\n\nprocessor : 1\nmodel name : Example\n").state
          == ReadState::Unavailable, "Missing per-CPU feature list prevents an apparently complete result");
    check(parseCpuInstructionSets("processor : 0\nflags : sse\n\nprocessor : 0\nflags : avx\n").error == EINVAL,
          "Duplicate CPU records are rejected");
    check(parseCpuInstructionSets("Hardware : Board\nFeatures : aes\n").state == ReadState::Unavailable,
          "Global board fields are not assigned to a processor");
    check(parseCpuInstructionSets(QString(4 * 1024 * 1024 + 1, 'x')).error == EOVERFLOW,
          "CPU feature parser input is bounded");
    QHash<QString, QString> memory {
        {"MEMORY_ARRAY_NUM_DEVICES", "2"},
        {"MEMORY_DEVICE_0_SIZE", "17179869184"},
        {"MEMORY_DEVICE_1_SIZE", "17179869184"}
    };
    check(parseInstalledMemory(memory).value == "34359738368", "Two 16 GiB devices report 32 GiB installed");
    memory["MEMORY_ARRAY_NUM_DEVICES"] = "3";
    memory["MEMORY_DEVICE_2_PRESENT"] = "0";
    check(parseInstalledMemory(memory).value == "34359738368", "Explicitly empty slot does not invalidate total");
    memory.remove("MEMORY_DEVICE_2_PRESENT");
    check(parseInstalledMemory(memory).state == ReadState::Unavailable, "Unknown slot never produces a partial total");
    memory["MEMORY_DEVICE_2_SIZE"] = "garbage";
    check(parseInstalledMemory(memory).state == ReadState::Error, "Malformed capacity is a read error");
    memory["MEMORY_DEVICE_2_SIZE"] = "18446744073709551615";
    check(parseInstalledMemory(memory).error == EOVERFLOW, "Installed capacity sum checks overflow");
    memory["MEMORY_DEVICE_2_PRESENT"] = "0";
    check(parseInstalledMemory(memory).state == ReadState::Error, "Nonzero size conflicts with empty slot evidence");
    memory.remove("MEMORY_DEVICE_2_SIZE");
    memory["MEMORY_DEVICE_0_NON_VOLATILE_SIZE"] = "0";
    memory["MEMORY_DEVICE_1_NON_VOLATILE_SIZE"] = "Unknown";
    check(parseInstalledMemory(memory).value == "34359738368", "Unknown optional nonvolatile field preserves known capacities");
    memory["MEMORY_DEVICE_0_NON_VOLATILE_SIZE"] = "17179869184";
    check(parseInstalledMemory(memory).state == ReadState::Unsupported, "Persistent capacity is not silently counted as RAM");
    memory["MEMORY_ARRAY_NUM_DEVICES"] = "4097";
    check(parseInstalledMemory(memory).error == EOVERFLOW, "Memory slot count is bounded");
    check(parseInstalledMemory({}).state == ReadState::Unavailable, "Missing firmware data is not zero RAM");
    QHash<QString, QString> records {
        {"MEMORY_ARRAY_NUM_DEVICES", "3"}, {"MEMORY_DEVICE_0_SIZE", "17179869184"},
        {"MEMORY_DEVICE_0_LOCATOR", "Same locator"}, {"MEMORY_DEVICE_1_LOCATOR", "Same locator"},
        {"MEMORY_DEVICE_0_TYPE", "LPDDR5"}, {"MEMORY_DEVICE_0_SPEED_MTS", "8533"},
        {"MEMORY_DEVICE_0_CONFIGURED_SPEED_MTS", "0"}, {"MEMORY_DEVICE_0_PART_NUMBER", "Unknown"},
        {"MEMORY_DEVICE_1_SIZE", "Unknown"}, {"MEMORY_DEVICE_2_PRESENT", "0"}
    };
    auto details = parseMemoryDevices(records);
    check(details.status.state == ReadState::Available && details.devices.size() == 3,
          "Keep populated, unknown and explicitly empty firmware records separate");
    check(details.devices[0].values.value("PRESENT").state == ReadState::Unavailable
          && details.devices[0].values.value("SIZE").value == "17179869184",
          "Missing presence flag does not discard known capacity");
    check(details.devices[0].values.value("LOCATOR").value == details.devices[1].values.value("LOCATOR").value,
          "Duplicate locators do not merge records");
    check(details.devices[1].values.value("SIZE").state == ReadState::Unavailable
          && details.devices[2].values.value("SIZE").state == ReadState::NotApplicable,
          "Unknown capacity differs from explicitly empty device");
    check(details.devices[0].values.value("SPEED_MTS").value == "8533"
          && details.devices[0].values.value("CONFIGURED_SPEED_MTS").state == ReadState::Unavailable
          && details.devices[0].values.value("PART_NUMBER").state == ReadState::Unavailable,
          "MT/s remains numeric; zero rate and firmware placeholders remain unavailable");
    records["MEMORY_DEVICE_0_NON_VOLATILE_SIZE"] = "1073741824";
    records["MEMORY_DEVICE_0_VOLATILE_SIZE"] = "16106127360";
    records["MEMORY_DEVICE_1_SPEED_MTS"] = "bad";
    records["MEMORY_DEVICE_2_SIZE"] = "1024";
    details = parseMemoryDevices(records);
    check(details.devices[0].values.value("NON_VOLATILE_SIZE").value == "1073741824"
          && details.devices[0].values.value("VOLATILE_SIZE").value == "16106127360",
          "Volatile and persistent capacities remain separate");
    check(details.devices[1].values.value("SPEED_MTS").error == EINVAL
          && details.devices[2].values.value("SIZE").error == EINVAL
          && details.devices[2].values.value("PRESENT").error == EINVAL,
          "Malformed numeric values and contradictory empty records are explicit errors");
    records["MEMORY_DEVICE_0_MANUFACTURER"] = QString(257, 'x');
    check(parseMemoryDevices(records).status.error == EOVERFLOW
          && parseMemoryDevices(records).devices.isEmpty(), "Oversized metadata publishes no partial record list");
    records.remove("MEMORY_DEVICE_0_MANUFACTURER");
    records["MEMORY_ARRAY_NUM_DEVICES"] = "257";
    check(parseMemoryDevices(records).status.error == EOVERFLOW, "Detailed record expansion is bounded");
    check(parseMemoryDevices({}).status.state == ReadState::Unavailable, "Missing cache is not a permission failure");
    records = {{"MEMORY_ARRAY_NUM_DEVICES", "0"}};
    check(parseMemoryDevices(records).status.state == ReadState::Available
          && parseMemoryDevices(records).devices.isEmpty(), "Explicit zero record count is distinct from missing cache");
    check(parseSecureBoot(QByteArray(1, char(1))).value == "enabled", "SecureBoot byte enables firmware state");
    check(parseSecureBoot(QByteArray(1, char(0))).value == "disabled", "SecureBoot zero disables firmware state");
    check(parseSecureBoot({}).state == ReadState::Error
          && parseSecureBoot(QByteArray(1, char(2))).state == ReadState::Error
          && parseSecureBoot(QByteArray(5, char(0))).state == ReadState::Error,
          "Malformed SecureBoot payload is not treated as disabled");
    const auto signatureList = [](const QByteArray &guid, const QByteArray &signature) {
        QByteArray list = guid;
        const auto append = [&list](quint32 value) {
            char bytes[4];
            qToLittleEndian<quint32>(value, reinterpret_cast<uchar *>(bytes));
            list.append(bytes, 4);
        };
        append(28 + 16 + signature.size()); append(0); append(16 + signature.size());
        list.append(QByteArray(16, char(0))); // Signature-owner GUID.
        list.append(signature);
        return list;
    };
    const QByteArray x509Guid = QByteArray::fromHex("a159c0a5e494a74a87b5ab155c2bf072");
    const QByteArray der = QByteArray::fromBase64(
        "MIIBGzCBwaADAgECAgEBMAoGCCqGSM49BAMCMBcxFTATBgNVBAMMDFFMRE0gZml4dHVyZTAeFw0yMDAxMDEwMDAwMDBaFw0zMDAxMDEwMDAwMDBaMBcxFTATBgNVBAMMDFFMRE0gZml4dHVyZTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABK9KZsQWrAixDhWcxerBMOstANWdiM9RdfPf+25bsZX6Hgz+pMLR2hpgxDWrLCpmGDVZNYAAya1/3hrgz6/Tm9gwCgYIKoZIzj0EAwIDSQAwRgIhAPjy8A6F++gXv0CNHw3xd5VwAHI0/Tpt5Bifyhf1KK5AAiEA839IXhcKhZWx329x4fNC1e8wt25T7EBf5Hjzq6wDviw=");
    const QByteArray list = signatureList(x509Guid, der);
    const MokCertificates mok = parseMokCertificates(list);
    check(mok.status.state == ReadState::Available && mok.certificates.size() == 1
          && mok.certificates.first().subject.contains("QLDM fixture")
          && mok.certificates.first().issuer.contains("QLDM fixture")
          && mok.certificates.first().expires == "2030-01-01T00:00:00Z",
          "MOK X.509 owner, issuer and expiration remain paired");
    check(parseMokCertificates(list + list).certificates.size() == 2, "Multiple MOK certificate lists are retained");
    check(parseMokCertificates(list.left(list.size() - 1)).status.state == ReadState::Error,
          "Truncated MOK lists cannot expose apparently complete certificates");
    check(parseMokCertificates(signatureList(x509Guid, "invalid DER")).status.state == ReadState::Error,
          "Invalid X.509 data does not become an empty successful list");
    const MokCertificates hashes = parseMokCertificates(signatureList(QByteArray(16, char(0)), QByteArray(32, char(1))));
    check(hashes.status.state == ReadState::Available && hashes.certificates.isEmpty() && hashes.otherSignatures == 1,
          "Non-certificate signatures are counted without inventing certificate dates");
    check(parseMokCertificates(QByteArray(1024 * 1024 + 1, char(0))).status.error == EOVERFLOW,
          "MOK input is bounded");
    return failures ? 1 : 0;
}
