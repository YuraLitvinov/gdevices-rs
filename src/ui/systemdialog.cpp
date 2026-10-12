#include "systemdialog.h"
#include "propertytext.h"
#include <QApplication>
#include <QThread>
#include <QClipboard>
#include <QDateTime>
#include <QDialogButtonBox>
#include <QGridLayout>
#include <QFont>
#include <QSizePolicy>
#include <QTabWidget>
#include <QLabel>
#include <QLocale>
#include <QPushButton>
#include <QScrollArea>
#include <QVBoxLayout>
#include <QVector>
#include <utility>
#include <limits>

namespace {
QVector<QPair<QString, QString>> memoryFields()
{
    using D = SystemDialog;
    return {{"PRESENT", D::tr("Presence")}, {"SIZE", D::tr("Capacity (total)")},
        {"VOLATILE_SIZE", D::tr("Volatile capacity")}, {"NON_VOLATILE_SIZE", D::tr("Nonvolatile capacity")},
        {"LOCATOR", D::tr("Device locator")}, {"BANK_LOCATOR", D::tr("Bank locator")},
        {"TYPE", D::tr("Memory type")}, {"FORM_FACTOR", D::tr("Form factor")},
        {"SPEED_MTS", D::tr("Rated transfer rate")}, {"CONFIGURED_SPEED_MTS", D::tr("Configured transfer rate")},
        {"MANUFACTURER", D::tr("Manufacturer")}, {"PART_NUMBER", D::tr("Part number")}};
}
QString memoryScope()
{
    return SystemDialog::tr("Firmware-reported records; these do not establish removable modules, physical slots, "
        "memory channels, upgradeability or active ECC. Transfer rates are firmware declarations, not live measurements.");
}
}

SystemDialog::SystemDialog(QWidget *parent) : QDialog(parent)
{
    setWindowTitle(tr("System Information"));
    resize(680, 650);
    auto *layout = new QVBoxLayout(this);
    auto *tabs = new QTabWidget(this);
    tabs->setUsesScrollButtons(true);
    layout->addWidget(tabs, 1);
    QVector<QGridLayout *> forms;
    QVector<QLabel *> names;
    const auto group = [&](const QString &title) {
        auto *scroll = new QScrollArea(tabs);
        scroll->setWidgetResizable(true);
        auto *page = new QWidget(scroll);
        auto *contents = new QVBoxLayout(page);
        auto *fields = new QWidget(page);
        auto *form = new QGridLayout(fields);
        form->setContentsMargins(0, 0, 0, 0);
        contents->addWidget(fields);
        contents->addStretch();
        scroll->setWidget(page);
        tabs->addTab(scroll, title);
        form->setColumnStretch(1, 1);
        forms.append(form);
        return form;
    };
    const auto field = [&](QGridLayout *form, const QString &key, const QString &caption) {
        auto *name = new QLabel(caption, form->parentWidget());
        name->setTextFormat(Qt::PlainText);
        names.append(name);
        auto *value = new QLabel(tr("Not collected"), form->parentWidget());
        value->setTextFormat(Qt::PlainText);
        value->setTextInteractionFlags(Qt::TextSelectableByMouse | Qt::TextSelectableByKeyboard);
        QSizePolicy policy(QSizePolicy::Expanding, QSizePolicy::Preferred);
        policy.setHeightForWidth(true);
        value->setSizePolicy(policy);
        value->setWordWrap(true);
        value->setAlignment(Qt::AlignLeading | Qt::AlignTop);
        value->setMinimumWidth(120);
        const int row = form->count() / 2;
        form->addWidget(name, row, 0, Qt::AlignLeading | Qt::AlignTop);
        form->addWidget(value, row, 1);
        fields_.insert(key, value);
        captions_.insert(key, caption);
        order_.append(key);
    };
    auto *os = group(tr("System"));
    field(os, "hostname", tr("Hostname"));
    field(os, "os", tr("Operating system"));
    field(os, "architecture", tr("Kernel architecture"));
    field(os, "kernel", tr("Kernel version"));
    field(os, "build", tr("Kernel build"));
    field(os, "boot", tr("Boot mode"));
    field(os, "virtualization", tr("Virtualization environment"));
    field(os, "uptime", tr("Uptime"));
    auto *secureBoot = group(tr("Secure Boot"));
    field(secureBoot, "secure_boot", tr("Status"));
    captions_.insert("secure_boot", tr("Secure Boot"));
    auto *mokTitle = new QLabel(tr("MOK certificates"), secureBoot->parentWidget());
    mokTitle->setTextFormat(Qt::PlainText);
    QFont headingFont = mokTitle->font();
    headingFont.setBold(true);
    mokTitle->setFont(headingFont);
    secureBoot->addWidget(mokTitle, 1, 0, 1, 2);
    auto *mokPage = new QWidget(secureBoot->parentWidget());
    mokForm_ = new QGridLayout(mokPage);
    mokForm_->setContentsMargins(0, 0, 0, 0);
    mokForm_->setColumnStretch(1, 1);
    secureBoot->addWidget(mokPage, 2, 0, 1, 2);
    order_.append("mok");
    captions_.insert("mok", tr("MOK certificates"));
    auto *hardware = group(tr("Hardware and firmware"));
    field(hardware, "manufacturer", tr("System manufacturer"));
    field(hardware, "model", tr("System model"));
    field(hardware, "board_vendor", tr("Motherboard manufacturer"));
    field(hardware, "board_model", tr("Motherboard model"));
    field(hardware, "firmware", tr("Firmware version"));
    field(hardware, "firmware_date", tr("Firmware date (reported)"));
    field(hardware, "physical_memory", tr("Physical RAM"));
    field(hardware, "memory", tr("Usable RAM"));
    auto *processor = group(tr("Processor"));
    field(processor, "cpu", tr("CPU model(s)"));
    field(processor, "sockets", tr("Sockets (reported topology)"));
    field(processor, "cores", tr("Physical cores (reported topology)"));
    field(processor, "logical", tr("Logical CPUs present"));
    field(processor, "online", tr("Logical CPUs online"));
    field(processor, "caches", tr("CPU caches (online CPUs)"));
    field(processor, "instruction_sets", tr("Instruction sets / CPU features"));
    memoryForm_ = group(tr("RAM/Memory"));
    order_.append("memory_devices");
    captions_.insert("memory_devices", tr("RAM/Memory"));
    // Use one font-derived caption width for every section, so values align.
    int captionWidth = 0;
    for (const QLabel *name : names) captionWidth = qMax(captionWidth, name->sizeHint().width());
    for (const QString &caption : {tr("Owner:"), tr("Issuer:"), tr("Expires:")}) {
        const QLabel name(caption);
        captionWidth = qMax(captionWidth, name.sizeHint().width());
    }
    for (const auto &entry : memoryFields()) {
        const QLabel name(entry.second);
        captionWidth = qMax(captionWidth, name.sizeHint().width());
    }
    for (QGridLayout *form : forms) form->setColumnMinimumWidth(0, captionWidth);
    mokForm_->setColumnMinimumWidth(0, captionWidth);
    rebuildMok();
    rebuildMemory();
    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Close, this);
    copy_ = buttons->addButton(tr("Copy all"), QDialogButtonBox::ActionRole);
    refresh_ = buttons->addButton(tr("Refresh"), QDialogButtonBox::ActionRole);
    copy_->setEnabled(false);
    connect(copy_, &QPushButton::clicked, this, &SystemDialog::copyAll);
    connect(refresh_, &QPushButton::clicked, this, &SystemDialog::refreshRequested);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::close);
    layout->addWidget(buttons);
}
void SystemDialog::setBusy(bool busy)
{
    refresh_->setEnabled(!busy);
    refresh_->setText(busy ? tr("Refreshing…") : tr("Refresh"));
}
QString SystemDialog::displayValue(const QString &key, const Attribute &value) const
{
    if (key == "virtualization" && value.state == ReadState::Unavailable)
        return tr("Unknown (no hypervisor identity reported)");
    if (key == "memory_devices") {
        QStringList lines {memoryScope()};
        const auto &memory = snapshot_.memoryDevices;
        if (memory.status.state != ReadState::Available) lines.append(propertyReadValue(memory.status));
        else if (memory.devices.isEmpty()) lines.append(tr("No memory-device records reported"));
        else for (const FirmwareMemoryDevice &device : memory.devices) {
            lines.append(tr("Memory device %1 (firmware record %2)").arg(device.index + 1).arg(device.index));
            for (const auto &field : memoryFields())
                lines.append(field.second + ": " + memoryValue(field.first, device.values.value(field.first)));
        }
        return lines.join('\n');
    }
    if (key == "mok") {
        const MokCertificates &mok = snapshot_.mok;
        if (mok.status.state != ReadState::Available) return propertyReadValue(mok.status);
        QStringList certificates;
        const auto known = [this](const QString &text) { return text.isEmpty() ? tr("Unavailable") : text; };
        for (const MokCertificate &certificate : mok.certificates)
            certificates.append(tr("Owner: %1\nIssuer: %2\nExpires: %3")
                .arg(known(certificate.subject), known(certificate.issuer), known(certificate.expires)));
        if (certificates.isEmpty()) certificates.append(tr("No X.509 certificates in the exposed MOK list"));
        if (mok.otherSignatures) certificates.append(tr("%1 non-certificate signatures omitted").arg(mok.otherSignatures));
        return certificates.join("\n\n");
    }
    if (value.state != ReadState::Available) return propertyReadValue(value);
    if (key == "secure_boot") return value.value == "enabled" ? tr("Enabled") : tr("Disabled");
    if (key == "boot")
        return value.value == "uefi" ? tr("UEFI")
            : tr("UEFI not exposed (legacy boot or restricted environment)");
    if (key == "virtualization") return tr("Reported hypervisor: %1").arg(value.value);
    if (key == "memory" || key == "physical_memory") {
        constexpr qulonglong gib = 1024ULL * 1024 * 1024;
        constexpr qulonglong mib = 1024ULL * 1024;
        const qulonglong bytes = value.value.toULongLong();
        const int precision = key == "physical_memory" && bytes % gib == 0 ? 0 : 2;
        QString text = tr("%1 GiB").arg(QLocale().toString(bytes / static_cast<double>(gib), 'f', precision));
        const Attribute physical = snapshot_.values.value("physical_memory");
        if (key == "memory" && physical.state == ReadState::Available) {
            const qulonglong installed = physical.value.toULongLong();
            if (installed >= bytes)
                text += tr(" (%1 MiB is system reserved)")
                    .arg(QLocale().toString((installed - bytes) / static_cast<double>(mib), 'f', 0));
        }
        return text;
    }
    if (key == "uptime") {
        const qulonglong seconds = value.value.toULongLong();
        return tr("%1 days, %2 hours, %3 minutes").arg(seconds / 86400).arg(seconds / 3600 % 24).arg(seconds / 60 % 60);
    }
    if (key == "instruction_sets") {
        const QStringList groups = value.value.split('\n');
        QStringList lines;
        for (const QString &group : groups) {
            const QStringList parts = group.split('\t');
            lines.append(groups.size() == 1 ? parts.value(1)
                : tr("Logical CPUs %1: %2").arg(parts.value(0), parts.value(1)));
        }
        return lines.join("\n\n");
    }
    if (key == "caches") {
        QStringList lines;
        for (const QString &raw : value.value.split('\n')) {
            const QStringList parts = raw.split('\t');
            const QString kind = parts.value(0);
            QString type = kind.section(' ', 1);
            if (type == "Data") type = tr("data");
            else if (type == "Instruction") type = tr("instruction");
            else if (type == "Unified") type = tr("unified");
            lines.append(tr("%1 %2: %3 (%4 unique instances)")
                .arg(kind.section(' ', 0, 0), type,
                     QLocale().formattedDataSize(parts.value(1).toLongLong()), parts.value(2)));
        }
        return lines.join('\n');
    }
    return value.value;
}
QString SystemDialog::memoryValue(const QString &key, const Attribute &value) const
{
    if (key == "PRESENT" && value.state == ReadState::Unavailable)
        return tr("Unknown (no explicit presence flag)");
    if (value.state != ReadState::Available) return propertyReadValue(value);
    if (key == "PRESENT") return value.value == "0" ? tr("No device reported present") : tr("Device reported present");
    if (key == "SPEED_MTS" || key == "CONFIGURED_SPEED_MTS")
        return tr("%1 MT/s").arg(QLocale().toString(value.value.toULongLong()));
    if (key == "SIZE" || key == "VOLATILE_SIZE" || key == "NON_VOLATILE_SIZE") {
        const qulonglong bytes = value.value.toULongLong();
        if (bytes <= static_cast<qulonglong>(std::numeric_limits<qlonglong>::max()))
            return QLocale().formattedDataSize(static_cast<qlonglong>(bytes), 2, QLocale::DataSizeIecFormat);
        return tr("%1 bytes").arg(QLocale().toString(bytes));
    }
    return value.value;
}
void SystemDialog::rebuildMemory()
{
    while (QLayoutItem *item = memoryForm_->takeAt(0)) {
        delete item->widget();
        delete item;
    }
    int row = 0;
    const auto label = [this](const QString &text) {
        auto *value = new QLabel(text, memoryForm_->parentWidget());
        value->setTextFormat(Qt::PlainText);
        value->setWordWrap(true);
        value->setAlignment(Qt::AlignLeading | Qt::AlignTop);
        value->setTextInteractionFlags(Qt::TextSelectableByMouse | Qt::TextSelectableByKeyboard);
        QSizePolicy policy(QSizePolicy::Expanding, QSizePolicy::Preferred);
        policy.setHeightForWidth(true);
        value->setSizePolicy(policy);
        value->setMinimumWidth(120);
        return value;
    };
    const auto message = [&](const QString &text, bool heading = false) {
        auto *value = label(text);
        if (heading) { QFont font = value->font(); font.setBold(true); value->setFont(font); }
        memoryForm_->addWidget(value, row++, 0, 1, 2);
    };
    const FirmwareMemoryInformation &memory = snapshot_.memoryDevices;
    if (memory.status.state != ReadState::Available) {
        auto *value = label(propertyReadValue(memory.status));
        value->setToolTip("<qt>" + snapshot_.sources.value("memory_devices").toHtmlEscaped() + "</qt>");
        memoryForm_->addWidget(value, row, 0, 1, 2);
        return;
    }
    if (memory.devices.isEmpty()) message(tr("No memory-device records reported"));
    for (const FirmwareMemoryDevice &device : memory.devices) {
        message(tr("Memory device %1").arg(device.index + 1), true);
        const QString prefix = QStringLiteral("MEMORY_DEVICE_%1_").arg(device.index);
        for (const auto &field : memoryFields()) {
            auto *name = new QLabel(field.second, memoryForm_->parentWidget());
            name->setTextFormat(Qt::PlainText);
            auto *value = label(memoryValue(field.first, device.values.value(field.first)));
            const QString hint = "<qt>" + (QStringLiteral("libudev: dmi/id ") + prefix + field.first
                + '\n' + memoryScope()).toHtmlEscaped().replace('\n', "<br>") + "</qt>";
            name->setToolTip(hint); value->setToolTip(hint);
            memoryForm_->addWidget(name, row, 0, Qt::AlignLeading | Qt::AlignTop);
            memoryForm_->addWidget(value, row++, 1);
        }
    }
}
void SystemDialog::rebuildMok()
{
    while (QLayoutItem *item = mokForm_->takeAt(0)) {
        delete item->widget();
        delete item;
    }
    const MokCertificates &mok = snapshot_.mok;
    const QString scope = snapshot_.sources.value("mok") + '\n'
        + tr("Exposed runtime MOK certificates. Issuer and expiration are certificate metadata, "
             "not verification of the signer or proof that this key signed the running kernel. Dates are UTC. "
             "Firmware db certificates and pending enrollments are not included.");
    int row = 0;
    const auto label = [this](const QString &text) {
        auto *value = new QLabel(text, mokForm_->parentWidget());
        value->setTextFormat(Qt::PlainText);
        value->setWordWrap(true);
        value->setTextInteractionFlags(Qt::TextSelectableByMouse | Qt::TextSelectableByKeyboard);
        value->setAlignment(Qt::AlignLeading | Qt::AlignTop);
        QSizePolicy policy(QSizePolicy::Expanding, QSizePolicy::Preferred);
        policy.setHeightForWidth(true);
        value->setSizePolicy(policy);
        value->setMinimumWidth(120);
        return value;
    };
    const auto hint = [&scope](QLabel *value, const QString &details) {
        value->setToolTip("<qt>" + (scope + "\n\n" + details).toHtmlEscaped().replace('\n', "<br>") + "</qt>");
    };
    const auto message = [&](const QString &text) {
        auto *value = label(text);
        hint(value, {});
        mokForm_->addWidget(value, row++, 0, 1, 2);
    };
    if (mok.status.state != ReadState::Available || mok.certificates.isEmpty()) {
        message(displayValue("mok", {}));
        return;
    }
    const auto known = [this](const QString &text) { return text.isEmpty() ? tr("Unavailable") : text; };
    int index = 0;
    for (const MokCertificate &certificate : mok.certificates) {
        if (mok.certificates.size() > 1) message(tr("Certificate %1").arg(++index));
        const auto field = [&](const QString &caption, const QString &text, const QString &details) {
            auto *name = new QLabel(caption, mokForm_->parentWidget());
            name->setTextFormat(Qt::PlainText);
            auto *value = label(text);
            hint(name, details);
            hint(value, details);
            mokForm_->addWidget(name, row, 0, Qt::AlignLeading | Qt::AlignTop);
            mokForm_->addWidget(value, row++, 1);
        };
        const QString owner = certificate.subjectDisplay.isEmpty() ? certificate.subject : certificate.subjectDisplay;
        const QString issuer = !certificate.subject.isEmpty() && certificate.subject == certificate.issuer
            ? tr("Same as owner")
            : (certificate.issuerDisplay.isEmpty() ? certificate.issuer : certificate.issuerDisplay);
        const QDateTime expires = QDateTime::fromString(certificate.expires, Qt::ISODate);
        const QString date = expires.isValid()
            ? tr("%1 UTC").arg(QLocale().toString(expires.toUTC(), "d MMM yyyy, HH:mm:ss")) : known(certificate.expires);
        field(tr("Owner:"), known(owner), certificate.subject);
        field(tr("Issuer:"), known(issuer), certificate.issuer);
        field(tr("Expires:"), date, certificate.expires);
    }
    if (mok.otherSignatures)
        message(tr("%1 non-certificate signatures omitted").arg(mok.otherSignatures));
}
void SystemDialog::acceptResult(SystemProperties result)
{
    Q_ASSERT(QThread::isMainThread());
    snapshot_ = std::move(result);
    for (const QString &key : order_) {
        if (key == "mok" || key == "memory_devices") continue; // Variable-length sections are rebuilt separately.
        fields_.value(key)->setText(displayValue(key, snapshot_.values.value(key)));
        QString source = snapshot_.sources.value(key);
        if (key == "secure_boot")
            source += '\n' + tr("Firmware SecureBoot state; does not establish shim validation or kernel lockdown policy.");
        if (key == "instruction_sets")
            source += '\n' + tr("Kernel-exposed instruction-set and CPU-feature flags for processors listed in /proc/cpuinfo. "
                "These include non-instruction features and may be limited by the kernel, operating system or hypervisor. "
                "Different reported feature lists remain separate; this is not a complete raw hardware capability inventory.");
        if (key == "memory")
            source += '\n' + tr("System reserved is reported physical RAM minus Linux MemTotal. "
                                 "It includes all memory unavailable to Linux, not just firmware reservations. "
                                 "It is omitted if physical capacity is unavailable or smaller than usable RAM.");
        fields_.value(key)->setToolTip("<qt>" + source.toHtmlEscaped().replace('\n', "<br>") + "</qt>");
    }
    rebuildMok();
    rebuildMemory();
    setBusy(false);
    copy_->setEnabled(true);
}
void SystemDialog::copyAll()
{
    QStringList lines;
    for (const QString &key : order_)
        lines.append(captions_.value(key) + ": " + displayValue(key, snapshot_.values.value(key)));
    QApplication::clipboard()->setText(lines.join('\n'));
}
