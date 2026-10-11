#include "resourceformat.h"
#include "propertiesdialog.h"
#include "devicelabel.h"
#include <QApplication>
#include <QThread>
#include <QCoreApplication>
#include <QCheckBox>
#include <QClipboard>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFontDatabase>
#include <QFileInfo>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QGridLayout>
#include <QGroupBox>
#include <QSizePolicy>
#include <QItemSelectionModel>
#include <QLabel>
#include <QLocale>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QScrollArea>
#include <QSignalBlocker>
#include <QStringList>
#include <QTabWidget>
#include <QTableWidget>
#include <algorithm>
#include <cerrno>
#include <QVBoxLayout>
#include <utility>

namespace {
QString eventDescription(const DeviceEvent &event)
{
    if (event.action == "add") return QCoreApplication::translate("PropertiesDialog", "Device addition observed");
    if (event.action == "remove") return QCoreApplication::translate("PropertiesDialog", "Device removal observed");
    if (event.action == "change") return QCoreApplication::translate("PropertiesDialog", "Device change reported");
    if (event.action == "bind") return QCoreApplication::translate("PropertiesDialog", "Driver binding reported");
    if (event.action == "unbind") return QCoreApplication::translate("PropertiesDialog", "Driver unbinding reported");
    if (event.action == "move") return QCoreApplication::translate("PropertiesDialog", "Device path move reported");
    if (event.action == "online") return QCoreApplication::translate("PropertiesDialog", "Device online event reported");
    if (event.action == "offline") return QCoreApplication::translate("PropertiesDialog", "Device offline event reported");
    return QCoreApplication::translate("PropertiesDialog", "udev action: %1").arg(event.action);
}
QString eventText(const DeviceEvent &event)
{
    const auto value = [](const QString &text) {
        return text.isEmpty() ? QCoreApplication::translate("PropertiesDialog", "Unavailable in event payload") : text;
    };
    // Format each field once so a literal "%9" in a device string cannot
    // become a later QString::arg placeholder.
    return QStringList {
        QCoreApplication::translate("PropertiesDialog", "Observed: %1").arg(event.observed.toString(Qt::ISODateWithMs)),
        QCoreApplication::translate("PropertiesDialog", "Elapsed since monitor startup: %1 ms").arg(event.elapsedMs),
        QCoreApplication::translate("PropertiesDialog", "Source: live udev observation"),
        QCoreApplication::translate("PropertiesDialog", "Action: %1").arg(value(event.action)),
        QCoreApplication::translate("PropertiesDialog", "Description: %1").arg(eventDescription(event)),
        QCoreApplication::translate("PropertiesDialog", "Kernel path: %1").arg(event.path),
        QCoreApplication::translate("PropertiesDialog", "Subsystem: %1").arg(value(event.subsystem)),
        QCoreApplication::translate("PropertiesDialog", "Device type: %1").arg(value(event.devtype)),
        QCoreApplication::translate("PropertiesDialog", "Driver in event payload: %1").arg(value(event.driver)),
        QCoreApplication::translate("PropertiesDialog", "udev sequence: %1").arg(event.sequence ? QString::number(event.sequence) : value(QString())),
        QCoreApplication::translate("PropertiesDialog", "Initialization stamp: %1").arg(value(event.initialized)),
        QCoreApplication::translate("PropertiesDialog", "Local instance token: %1").arg(event.instance),
        QCoreApplication::translate("PropertiesDialog", "Previous path: %1").arg(value(event.oldPath))
    }.join('\n');
}
void clearForm(QFormLayout *form)
{
    while (form->rowCount()) form->removeRow(0);
}
QLabel *plainLabel(const QString &text, QWidget *parent)
{
    auto *label = new QLabel(text, parent);
    label->setTextFormat(Qt::PlainText);
    label->setWordWrap(true);
    label->setTextInteractionFlags(Qt::TextSelectableByMouse | Qt::TextSelectableByKeyboard);
    label->setFocusPolicy(Qt::StrongFocus);
    return label;
}
QFormLayout *storageDetailsForm(QVBoxLayout *layout, QWidget *parent)
{
    auto *scroll = new QScrollArea(parent);
    scroll->setWidgetResizable(true);
    scroll->setFrameShape(QFrame::StyledPanel);
    auto *page = new QWidget(scroll);
    auto *form = new QFormLayout(page);
    form->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
    form->setRowWrapPolicy(QFormLayout::WrapLongRows);
    form->setFormAlignment(Qt::AlignTop);
    scroll->setWidget(page);
    layout->addWidget(scroll, 1);
    return form;
}
void showStorageFields(QFormLayout *form, const QVector<StorageDisplayField> &fields)
{
    clearForm(form);
    for (const StorageDisplayField &field : fields) {
        auto *caption = plainLabel(field.label + ':', form->parentWidget());
        auto *value = plainLabel(field.value, form->parentWidget());
        const QString hint = "<qt>" + field.source.toHtmlEscaped().replace('\n', "<br>") + "</qt>";
        caption->setToolTip(hint);
        value->setToolTip(hint);
        form->addRow(caption, value);
    }
}
QString selectedStorageText(const QVector<StorageDisplayRow> &rows, int index)
{
    if (index < 0 || index >= rows.size()) return {};
    const StorageDisplayRow &row = rows[index];
    return row.name + '\n' + storageFieldsText(row.fields);
}
}

PropertiesDialog::PropertiesDialog(const Device &device, QWidget *parent) : QDialog(parent)
{
    snapshot_.device = device;
    setAttribute(Qt::WA_DeleteOnClose);
    setWindowTitle(tr("%1 — Properties").arg(deviceDisplayName(device)));
    resize(680, 560);
    auto *layout = new QVBoxLayout(this);
    banner_ = plainLabel(tr("Loading properties…"), this);
    layout->addWidget(banner_);
    tabs_ = new QTabWidget(this);
    auto *tabs = tabs_;
    layout->addWidget(tabs, 1);
    const auto addForm = [tabs](const QString &title) {
        auto *scroll = new QScrollArea(tabs);
        scroll->setWidgetResizable(true);
        scroll->setFrameShape(QFrame::NoFrame);
        auto *page = new QWidget(scroll);
        auto *form = new QFormLayout(page);
        form->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
        form->setRowWrapPolicy(QFormLayout::WrapLongRows);
        scroll->setWidget(page);
        tabs->addTab(scroll, title);
        return form;
    };
    general_ = addForm(tr("General"));
    driver_ = addForm(tr("Driver"));
    networkPage_ = new QWidget(tabs);
    networkPage_->hide();
    auto *networkLayout = new QVBoxLayout(networkPage_);
    auto *networkNotice = plainLabel(tr("ℹ️ Network interface snapshot"), networkPage_);
    networkNotice->setToolTip(tr("Addresses belong to the application's network namespace. Reload properties to refresh. "
        "Hardware MAC is the permanent address reported by the driver. An overridden MAC is the current "
        "address when it differs from the permanent address, or Linux reports a random, inherited or explicitly set address."));
    networkLayout->addWidget(networkNotice);
    auto *networkScroll = new QScrollArea(networkPage_);
    networkScroll->setWidgetResizable(true);
    auto *networkContent = new QWidget(networkScroll);
    networkGroups_ = new QVBoxLayout(networkContent);
    networkGroups_->setSizeConstraint(QLayout::SetMinAndMaxSize);
    networkScroll->setWidget(networkContent);
    networkLayout->addWidget(networkScroll, 1);
    auto *copyNetwork = new QPushButton(tr("Copy network properties"), networkPage_);
    networkLayout->addWidget(copyNetwork);
    connect(copyNetwork, &QPushButton::clicked, this, [this] {
        QApplication::clipboard()->setText(networkText_);
    });
    displayPage_ = new QWidget(tabs);
    displayPage_->hide();
    auto *displayLayout = new QVBoxLayout(displayPage_);
    auto *displayNotice = plainLabel(tr("ℹ️ Desktop display configuration snapshot"), displayPage_);
    displayNotice->setToolTip(tr("Resolution and refresh rate describe the configured physical mode. "
        "Desktop color depth is not the panel or link depth. Scale excludes Qt application scaling. "
        "Wayland scale derived from logical size may include rounding; integer scale is a fallback. "
        "Reload properties to refresh. Unmatched or ambiguous connector names remain unavailable."));
    displayLayout->addWidget(displayNotice);
    auto *displayScroll = new QScrollArea(displayPage_);
    displayScroll->setWidgetResizable(true);
    auto *displayContent = new QWidget(displayScroll);
    displayGroups_ = new QVBoxLayout(displayContent);
    displayGroups_->setSizeConstraint(QLayout::SetMinAndMaxSize);
    displayScroll->setWidget(displayContent);
    displayLayout->addWidget(displayScroll, 1);
    auto *copyDisplay = new QPushButton(tr("Copy display properties"), displayPage_);
    displayLayout->addWidget(copyDisplay);
    connect(copyDisplay, &QPushButton::clicked, this, [this] {
        QApplication::clipboard()->setText(displayText_);
    });
    usbPage_ = new QWidget(tabs);
    usbPage_->hide();
    auto *usbLayout = new QVBoxLayout(usbPage_);
    auto *usbNotice = plainLabel(tr("ℹ️ USB configuration snapshot"), usbPage_);
    usbNotice->setToolTip(tr("Power information is declared by the active USB configuration, not measured consumption. "
        "USB version, connection speed and device identity remain on Details. Reload properties to refresh."));
    usbLayout->addWidget(usbNotice);
    auto *usbScroll = new QScrollArea(usbPage_);
    usbScroll->setWidgetResizable(true);
    auto *usbContent = new QWidget(usbScroll);
    usbGroups_ = new QVBoxLayout(usbContent);
    usbGroups_->setSizeConstraint(QLayout::SetMinAndMaxSize);
    usbScroll->setWidget(usbContent);
    usbLayout->addWidget(usbScroll, 1);
    auto *copyUsb = new QPushButton(tr("Copy USB properties"), usbPage_);
    usbLayout->addWidget(copyUsb);
    connect(copyUsb, &QPushButton::clicked, this, [this] {
        QApplication::clipboard()->setText(usbText_);
    });
    storagePage_ = new QWidget(tabs);
    storagePage_->hide();
    auto *storageLayout = new QVBoxLayout(storagePage_);
    storageNotice_ = plainLabel(tr("ℹ️ Storage snapshot for this device"), storagePage_);
    storageLayout->addWidget(storageNotice_);
    auto *storageScroll = new QScrollArea(storagePage_);
    storageScroll->setWidgetResizable(true);
    storageScroll->setFrameShape(QFrame::NoFrame);
    auto *storageFormPage = new QWidget(storageScroll);
    storage_ = new QFormLayout(storageFormPage);
    storage_->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
    storage_->setRowWrapPolicy(QFormLayout::WrapLongRows);
    storageScroll->setWidget(storageFormPage);
    storageLayout->addWidget(storageScroll, 1);
    auto *copyStorage = new QPushButton(tr("Copy storage overview"), storagePage_);
    storageLayout->addWidget(copyStorage);
    connect(copyStorage, &QPushButton::clicked, this, [this] {
        QApplication::clipboard()->setText(storageFieldsText(storagePresentation_.overview)
            + "\n\n" + storagePresentation_.notes);
    });

    volumesPage_ = new QWidget(tabs);
    volumesPage_->hide();
    auto *volumesLayout = new QVBoxLayout(volumesPage_);
    volumesNotice_ = plainLabel(tr("ℹ️ Related volumes and mounts"), volumesPage_);
    volumesLayout->addWidget(volumesNotice_);
    volumesTable_ = new QTableWidget(0, 5, volumesPage_);
    volumesTable_->setAlternatingRowColors(true);
    volumesTable_->setHorizontalHeaderLabels({tr("Device / label"), tr("Layer"), tr("Capacity"), tr("Content"), tr("Mount points")});
    volumesTable_->setEditTriggers(QAbstractItemView::NoEditTriggers);
    volumesTable_->setSelectionBehavior(QAbstractItemView::SelectRows);
    volumesTable_->setSelectionMode(QAbstractItemView::SingleSelection);
    volumesTable_->verticalHeader()->hide();
    volumesTable_->horizontalHeader()->setSectionResizeMode(QHeaderView::Interactive);
    volumesTable_->horizontalHeader()->setStretchLastSection(true);
    volumesTable_->setColumnWidth(0, 180);
    volumesTable_->setColumnWidth(1, 110);
    volumesTable_->setColumnWidth(2, 180);
    volumesLayout->addWidget(volumesTable_, 2);
    volumeDetails_ = storageDetailsForm(volumesLayout, volumesPage_);
    auto *volumeButtons = new QHBoxLayout;
    auto *copyVolume = new QPushButton(tr("Copy selected volume"), volumesPage_);
    auto *copyVolumes = new QPushButton(tr("Copy all volumes"), volumesPage_);
    volumeButtons->addWidget(copyVolume);
    volumeButtons->addWidget(copyVolumes);
    volumeButtons->addStretch();
    volumesLayout->addLayout(volumeButtons);
    connect(volumesTable_, &QTableWidget::currentCellChanged, this, [this] { showVolume(); });
    connect(copyVolume, &QPushButton::clicked, this, [this] {
        QApplication::clipboard()->setText(selectedStorageText(storagePresentation_.volumes, volumesTable_->currentRow()));
    });
    connect(copyVolumes, &QPushButton::clicked, this, [this] {
        QApplication::clipboard()->setText(storageRowsText(storagePresentation_.volumes));
    });

    healthPage_ = new QWidget(tabs);
    healthPage_->hide();
    auto *healthLayout = new QVBoxLayout(healthPage_);
    healthNotice_ = plainLabel(tr("ℹ️ Cached drive health"), healthPage_);
    healthLayout->addWidget(healthNotice_);
    auto *healthLabel = new QLabel(tr("&Drive:"), healthPage_);
    healthDrive_ = new QComboBox(healthPage_);
    healthDrive_->setSizeAdjustPolicy(QComboBox::AdjustToMinimumContentsLengthWithIcon);
    healthDrive_->setMinimumContentsLength(24);
    healthLabel->setBuddy(healthDrive_);
    healthLayout->addWidget(healthLabel);
    healthLayout->addWidget(healthDrive_);
    healthDetails_ = storageDetailsForm(healthLayout, healthPage_);
    auto *healthButtons = new QHBoxLayout;
    auto *copyHealth = new QPushButton(tr("Copy selected drive health"), healthPage_);
    auto *copyAllHealth = new QPushButton(tr("Copy all drive health"), healthPage_);
    healthButtons->addWidget(copyHealth);
    healthButtons->addWidget(copyAllHealth);
    healthButtons->addStretch();
    healthLayout->addLayout(healthButtons);
    connect(healthDrive_, &QComboBox::currentIndexChanged, this, &PropertiesDialog::showHealth);
    connect(copyHealth, &QPushButton::clicked, this, [this] {
        QApplication::clipboard()->setText(selectedStorageText(storagePresentation_.health, healthDrive_->currentIndex()));
    });
    connect(copyAllHealth, &QPushButton::clicked, this, [this] {
        QApplication::clipboard()->setText(storageRowsText(storagePresentation_.health));
    });
    auto *details = new QWidget(tabs);
    auto *detailsLayout = new QVBoxLayout(details);
    auto *propertyLabel = new QLabel(tr("&Property:"), details);
    property_ = new QComboBox(details);
    property_->setSizeAdjustPolicy(QComboBox::AdjustToMinimumContentsLengthWithIcon);
    property_->setMinimumContentsLength(24);
    propertyLabel->setBuddy(property_);
    detailsLayout->addWidget(propertyLabel);
    detailsLayout->addWidget(property_);
    value_ = new QPlainTextEdit(details);
    value_->setReadOnly(true);
    detailsLayout->addWidget(value_, 1);
    source_ = plainLabel(QString(), details);
    detailsLayout->addWidget(source_);
    advanced_ = new QCheckBox(tr("Show &advanced properties"), details);
    detailsLayout->addWidget(advanced_);
    auto *copyLayout = new QHBoxLayout;
    auto *copySelected = new QPushButton(tr("Copy selection"), details);
    auto *copyValue = new QPushButton(tr("Copy value"), details);
    auto *copyEverything = new QPushButton(tr("Copy all values"), details);
    copySelected->setEnabled(false);
    connect(value_, &QPlainTextEdit::copyAvailable, copySelected, &QPushButton::setEnabled);
    connect(copySelected, &QPushButton::clicked, value_, &QPlainTextEdit::copy);
    connect(copyValue, &QPushButton::clicked, this, [this] { QApplication::clipboard()->setText(value_->toPlainText()); });
    connect(copyEverything, &QPushButton::clicked, this, &PropertiesDialog::copyAll);
    for (auto *button : {copySelected, copyValue, copyEverything}) copyLayout->addWidget(button);
    copyLayout->addStretch();
    detailsLayout->addLayout(copyLayout);
    tabs->addTab(details, tr("Details"));
    resourcesPage_ = new QWidget(tabs);
    resourcesPage_->hide(); // Not a tab until metadata confirms applicability.
    auto *resourcesLayout = new QVBoxLayout(resourcesPage_);
    resourcesTable_ = new QTableWidget(0, 3, resourcesPage_);
    resourcesTable_->setHorizontalHeaderLabels({tr("Resource type"), tr("Setting"), tr("Details")});
    resourcesTable_->setEditTriggers(QAbstractItemView::NoEditTriggers);
    resourcesTable_->setSelectionBehavior(QAbstractItemView::SelectRows);
    resourcesTable_->setSelectionMode(QAbstractItemView::ExtendedSelection);
    resourcesTable_->verticalHeader()->hide();
    resourcesTable_->horizontalHeader()->setSectionResizeMode(QHeaderView::ResizeToContents);
    resourcesTable_->horizontalHeader()->setStretchLastSection(true);
    resourcesLayout->addWidget(resourcesTable_);
    auto *resourceButtons = new QHBoxLayout;
    auto *copyResourceSelection = new QPushButton(tr("Copy selection"), resourcesPage_);
    auto *copyResourceAll = new QPushButton(tr("Copy all resources"), resourcesPage_);
    copyResourceSelection->setEnabled(false);
    connect(resourcesTable_->selectionModel(), &QItemSelectionModel::selectionChanged, this,
        [this, copyResourceSelection] {
            copyResourceSelection->setEnabled(resourcesTable_->selectionModel()->hasSelection());
        });
    connect(copyResourceSelection, &QPushButton::clicked, this, [this] { copyResources(true); });
    connect(copyResourceAll, &QPushButton::clicked, this, [this] { copyResources(false); });
    resourceButtons->addWidget(copyResourceSelection);
    resourceButtons->addWidget(copyResourceAll);
    resourceButtons->addStretch();
    resourcesLayout->addLayout(resourceButtons);
    // The tab appears only after collection finds resource records or read errors.
    auto *eventsPage = new QWidget(tabs);
    auto *eventsLayout = new QVBoxLayout(eventsPage);
    eventsNotice_ = plainLabel(tr("ℹ️ Live udev events since the application started"), eventsPage);
    eventsLayout->addWidget(eventsNotice_);
    eventsTable_ = new QTableWidget(0, 3, eventsPage);
    eventsTable_->setHorizontalHeaderLabels({tr("Observed time"), tr("Event type"), tr("Description")});
    eventsTable_->setEditTriggers(QAbstractItemView::NoEditTriggers);
    eventsTable_->setSelectionBehavior(QAbstractItemView::SelectRows);
    eventsTable_->setSelectionMode(QAbstractItemView::ExtendedSelection);
    eventsTable_->verticalHeader()->hide();
    eventsTable_->horizontalHeader()->setSectionResizeMode(QHeaderView::ResizeToContents);
    eventsTable_->horizontalHeader()->setStretchLastSection(true);
    eventsLayout->addWidget(eventsTable_, 2);
    eventDetails_ = new QPlainTextEdit(eventsPage);
    eventDetails_->setReadOnly(true);
    eventsLayout->addWidget(eventDetails_, 1);
    auto *eventButtons = new QHBoxLayout;
    auto *copyEventSelection = new QPushButton(tr("Copy selection"), eventsPage);
    auto *copyEventAll = new QPushButton(tr("Copy all events"), eventsPage);
    copyEventSelection->setEnabled(false);
    connect(eventsTable_->selectionModel(), &QItemSelectionModel::selectionChanged, this,
        [this, copyEventSelection] {
            copyEventSelection->setEnabled(eventsTable_->selectionModel()->hasSelection());
        });
    connect(eventsTable_, &QTableWidget::currentCellChanged, this, [this] { showEvent(); });
    connect(copyEventSelection, &QPushButton::clicked, this, [this] { copyEvents(true); });
    connect(copyEventAll, &QPushButton::clicked, this, [this] { copyEvents(false); });
    eventButtons->addWidget(copyEventSelection);
    eventButtons->addWidget(copyEventAll);
    eventButtons->addStretch();
    eventsLayout->addLayout(eventButtons);
    tabs->addTab(eventsPage, tr("Events"));
    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Close, this);
    reload_ = buttons->addButton(tr("Reload properties"), QDialogButtonBox::ActionRole);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    connect(reload_, &QPushButton::clicked, this, &PropertiesDialog::reloadRequested);
    connect(property_, &QComboBox::currentIndexChanged, this, &PropertiesDialog::showDetail);
    connect(advanced_, &QCheckBox::toggled, this, &PropertiesDialog::rebuildDetails);
    layout->addWidget(buttons);
    rebuild();
}
void PropertiesDialog::setBusy(bool busy)
{
    reload_->setEnabled(!busy && !removed_);
    if (busy && !removed_) {
        banner_->setText(tr("Loading properties…"));
        banner_->show();
    }
}
void PropertiesDialog::acceptResult(DeviceProperties result)
{
    Q_ASSERT(QThread::isMainThread());
    if (removed_ || result.device.path != device().path || result.device.generation != device().generation) return;
    setBusy(false);
    if (result.state == ReadState::Removed) { markRemoved(); return; }
    if (result.state != ReadState::Available) {
        banner_->show();
        banner_->setText(tr("Properties could not be refreshed: %1. Showing the previous snapshot.")
            .arg(propertyReadValue({result.state, {}, result.error})));
        return;
    }
    snapshot_ = std::move(result);
    banner_->clear();
    banner_->hide();
    rebuild();
}
void PropertiesDialog::reconcileInstance(const Device *current)
{
    if (removed_) return;
    if (!current || current->generation != device().generation || current->incarnation != device().incarnation) {
        markRemoved();
        return;
    }
    // Keep the same-instance snapshot until an explicit property reload.
    // An inventory refresh must never silently retarget this dialog.
}
void PropertiesDialog::markRemoved()
{
    removed_ = true;
    eventScope_ += '\n' + tr("Removed/replaced device: retained events are a read-only instance snapshot.");
    eventsNotice_->setToolTip("<qt>" + eventScope_.toHtmlEscaped().replace('\n', "<br>") + "</qt>");
    reload_->setEnabled(false);
    banner_->show();
    banner_->setText(tr("Device removed or replaced. This dialog is a read-only snapshot; close it and reopen Properties for the current device."));
    rebuild();
}
void PropertiesDialog::rebuild()
{
    rebuildResources();
    entries_ = propertyEntries(snapshot_, removed_);
    clearForm(general_);
    clearForm(driver_);
    rebuildStorage();
    rebuildNetwork();
    rebuildDisplay();
    rebuildUsb();
    for (const PropertyEntry &entry : entries_) {
        if (entry.tab > 1) continue;
        QFormLayout *form = entry.tab == 0 ? general_ : driver_;
        auto *label = plainLabel(entry.value, this);
        label->setToolTip(entry.source.toHtmlEscaped());
        form->addRow(entry.label + ':', label);
    }
    rebuildDetails();
}
void PropertiesDialog::rebuildDetails()
{
    const QString previous = property_->currentData().toString();
    const QSignalBlocker blocker(property_);
    property_->clear();
    QVector<const PropertyEntry *> available, other;
    for (const PropertyEntry &entry : entries_) {
        if (!isDetailsProperty(entry, device()) || (entry.advanced && !advanced_->isChecked())) continue;
        if (detailsPropertyStatus(entry).isEmpty()) available.append(&entry);
        else other.append(&entry);
    }
    for (const PropertyEntry *entry : available) property_->addItem(entry->label, entry->id);
    if (!available.isEmpty() && !other.isEmpty()) property_->insertSeparator(property_->count());
    for (const PropertyEntry *entry : other)
        property_->addItem(tr("%1 (%2)").arg(entry->label, detailsPropertyStatus(*entry)), entry->id);
    const int index = property_->findData(previous);
    property_->setCurrentIndex(index < 0 ? 0 : index);
    showDetail();
}
void PropertiesDialog::showDetail()
{
    const QString id = property_->currentData().toString();
    for (const PropertyEntry &entry : entries_) {
        if (entry.id != id) continue;
        value_->setFont(id == "efi/hex" ? QFontDatabase::systemFont(QFontDatabase::FixedFont) : QApplication::font());
        value_->setLineWrapMode(id == "efi/hex" ? QPlainTextEdit::NoWrap : QPlainTextEdit::WidgetWidth);
        value_->setPlainText(entry.value);
        source_->setText(entry.source.isEmpty() ? QString() : tr("Source: %1").arg(entry.source));
        return;
    }
    value_->clear();
    source_->clear();
}
void PropertiesDialog::copyAll()
{
    QStringList values;
    for (const PropertyEntry &entry : entries_) {
        if (!isDetailsProperty(entry, device()) || (entry.advanced && !advanced_->isChecked())) continue;
        QString text = entry.label + ":\n" + entry.value;
        if (!entry.source.isEmpty()) text += '\n' + tr("Source: %1").arg(entry.source);
        values.append(text);
    }
    QApplication::clipboard()->setText(values.join("\n\n"));
}

void PropertiesDialog::rebuildResources()
{
    const DeviceResources &data = snapshot_.resources;
    const int existing = tabs_->indexOf(resourcesPage_);
    if (data.hasInformation() && existing < 0) tabs_->addTab(resourcesPage_, tr("Resources"));
    else if (!data.hasInformation() && existing >= 0) {
        tabs_->removeTab(existing);
        resourcesPage_->hide();
    }
    resourcesTable_->setRowCount(0);
    QStringList copy;
    const auto row = [this, &copy](const QString &type, const QString &setting,
                                 const QString &details, const QString &source) {
        const int index = resourcesTable_->rowCount();
        resourcesTable_->insertRow(index);
        const QStringList values {type, setting, details};
        for (int column = 0; column < values.size(); ++column) {
            auto *item = new QTableWidgetItem(values[column]);
            item->setToolTip(source.toHtmlEscaped());
            resourcesTable_->setItem(index, column, item);
        }
        resourcesTable_->item(index, 0)->setData(Qt::UserRole, source);
        copy.append(values.join('\t') + '\n' + tr("Source: %1").arg(source));
    };
    if (!data.pnpState.isEmpty())
        copy.append(tr("PnP device state: %1").arg(data.pnpState == "active" ? tr("Active") : tr("Disabled")));
    for (const ResourceRow &entry : resourceRows(data))
        row(entry.type, entry.setting, entry.details, entry.source);
    resourcesText_ = copy.join("\n\n");
}
void PropertiesDialog::copyResources(bool selectedOnly)
{
    QList<int> rows;
    if (selectedOnly) {
        for (const QModelIndex &index : resourcesTable_->selectionModel()->selectedRows()) rows.append(index.row());
        std::sort(rows.begin(), rows.end());
    } else {
        QApplication::clipboard()->setText(resourcesText_);
        return;
    }
    QStringList text;
    for (const int row : rows) {
        QStringList values;
        for (int column = 0; column < resourcesTable_->columnCount(); ++column)
            values.append(resourcesTable_->item(row, column)->text());
        text.append(values.join('\t') + '\n' + tr("Source: %1").arg(
            resourcesTable_->item(row, 0)->data(Qt::UserRole).toString()));
    }
    QApplication::clipboard()->setText(text.join("\n\n"));
}

void PropertiesDialog::rebuildNetwork()
{
    const bool applicable = device().subsystem == "net" || device().category == "network";
    const int existing = tabs_->indexOf(networkPage_);
    if (applicable && existing < 0) tabs_->insertTab(2, networkPage_, tr("Network"));
    else if (!applicable && existing >= 0) {
        tabs_->removeTab(existing);
        networkPage_->hide();
    }
    while (QLayoutItem *item = networkGroups_->takeAt(0)) {
        delete item->widget();
        delete item;
    }
    networkText_.clear();
    if (!applicable) return;
    QVector<QGridLayout *> forms;
    QVector<QLabel *> captions;
    QStringList copy;
    const auto group = [&](const QString &title) {
        auto *box = new QGroupBox(title, networkGroups_->parentWidget());
        auto *form = new QGridLayout(box);
        form->setColumnStretch(1, 1);
        forms.append(form);
        networkGroups_->addWidget(box);
        if (!copy.isEmpty()) copy.append(QString());
        copy.append(title);
        return form;
    };
    const auto field = [&](QGridLayout *form, const QString &caption, const QString &text, const QString &source) {
        auto *name = new QLabel(caption, form->parentWidget());
        name->setTextFormat(Qt::PlainText);
        captions.append(name);
        auto *value = plainLabel(text, form->parentWidget());
        QSizePolicy policy(QSizePolicy::Expanding, QSizePolicy::Preferred);
        policy.setHeightForWidth(true);
        value->setSizePolicy(policy);
        value->setAlignment(Qt::AlignLeading | Qt::AlignTop);
        value->setMinimumWidth(120);
        const QString hint = "<qt>" + source.toHtmlEscaped() + "</qt>";
        name->setToolTip(hint);
        value->setToolTip(hint);
        const int row = form->count() / 2;
        form->addWidget(name, row, 0, Qt::AlignLeading | Qt::AlignTop);
        form->addWidget(value, row, 1);
        copy.append(caption + ": " + text);
    };
    if (snapshot_.network.isEmpty()) {
        auto *form = group(tr("Network interfaces"));
        field(form, tr("Interfaces"), snapshot_.networkState.state == ReadState::Available
            ? tr("No network interfaces observed") : propertyReadValue(snapshot_.networkState), device().path);
    }
    for (const NetworkProperties &interface : snapshot_.network) {
        auto *link = group(tr("%1 — Link").arg(interface.name));
        const bool noMac = interface.linkType.state == ReadState::Available && interface.linkType.value != "1";
        const Attribute current = noMac ? Attribute {ReadState::NotApplicable, {}, 0} : interface.currentAddress;
        field(link, tr("Hardware MAC address"), propertyReadValue(interface.permanentAddress),
            tr("Driver permanent address: ETHTOOL_GPERMADDR"));
        field(link, tr("Current MAC address"), propertyReadValue(current), interface.path + "/address");
        QString overrideValue;
        if (noMac) overrideValue = tr("Not applicable");
        else if (current.state != ReadState::Available) overrideValue = propertyReadValue(current);
        else if (interface.permanentAddress.state == ReadState::Available)
            overrideValue = current.value.compare(interface.permanentAddress.value, Qt::CaseInsensitive) == 0
                ? tr("None") : current.value;
        else if (interface.assignmentType.state == ReadState::Available
                 && (interface.assignmentType.value == "1" || interface.assignmentType.value == "2"
                     || interface.assignmentType.value == "3")) overrideValue = current.value;
        else overrideValue = tr("Unavailable; permanent MAC could not be determined");
        field(link, tr("Overridden MAC address"), overrideValue,
            interface.path + "/address; " + interface.path + "/addr_assign_type; ETHTOOL_GPERMADDR");
        field(link, tr("MTU"), propertyReadValue(interface.mtu), interface.path + "/mtu");
        auto *addresses = group(tr("%1 — IP addresses").arg(interface.name));
        const QString source = tr("getifaddrs: application network namespace");
        if (interface.addresses.isEmpty()) {
            field(addresses, tr("IP addresses"), interface.addressState.state == ReadState::Available
                ? tr("None assigned") : propertyReadValue(interface.addressState), source);
        }
        for (const NetworkAddress &address : interface.addresses)
            field(addresses, address.family == "IPv4" ? tr("IPv4 address") : tr("IPv6 address"),
                address.address + '/' + propertyReadValue(address.netmask), source);
    }
    // Match System Information: one font-derived caption width across sections.
    int captionWidth = 0;
    for (const QLabel *caption : captions) captionWidth = qMax(captionWidth, caption->sizeHint().width());
    for (QGridLayout *form : forms) form->setColumnMinimumWidth(0, captionWidth);
    networkGroups_->addStretch();
    networkText_ = copy.join('\n');
}

void PropertiesDialog::rebuildDisplay()
{
    const bool applicable = device().subsystem == "drm" && device().sysname.contains('-');
    const int existing = tabs_->indexOf(displayPage_);
    if (applicable && existing < 0) tabs_->insertTab(2, displayPage_, tr("Display"));
    else if (!applicable && existing >= 0) {
        tabs_->removeTab(existing);
        displayPage_->hide();
    }
    while (QLayoutItem *item = displayGroups_->takeAt(0)) {
        delete item->widget();
        delete item;
    }
    displayText_.clear();
    if (!applicable) return;
    const DisplayProperties &data = snapshot_.display;
    QVector<QGridLayout *> forms;
    QVector<QLabel *> captions;
    QStringList copy;
    const auto group = [&](const QString &title) {
        auto *box = new QGroupBox(title, displayGroups_->parentWidget());
        auto *form = new QGridLayout(box);
        form->setColumnStretch(1, 1);
        forms.append(form);
        displayGroups_->addWidget(box);
        if (!copy.isEmpty()) copy.append(QString());
        copy.append(title);
        return form;
    };
    const auto field = [&](QGridLayout *form, const QString &caption, const QString &text, const QString &source = QString()) {
        auto *name = new QLabel(caption, form->parentWidget());
        name->setTextFormat(Qt::PlainText);
        captions.append(name);
        auto *value = plainLabel(text, form->parentWidget());
        QSizePolicy policy(QSizePolicy::Expanding, QSizePolicy::Preferred);
        policy.setHeightForWidth(true);
        value->setSizePolicy(policy);
        value->setAlignment(Qt::AlignLeading | Qt::AlignTop);
        value->setMinimumWidth(120);
        const QString hint = "<qt>" + source.toHtmlEscaped() + "</qt>";
        name->setToolTip(hint);
        value->setToolTip(hint);
        const int row = form->count() / 2;
        form->addWidget(name, row, 0, Qt::AlignLeading | Qt::AlignTop);
        form->addWidget(value, row, 1);
        copy.append(caption + ": " + text);
    };
    if (data.status.state == ReadState::Available && data.status.value == "disconnected") {
        auto *session = group(tr("Desktop session"));
        field(session, tr("Configuration"), tr("Disconnected"), data.sources.value("configuration"));
        displayGroups_->addStretch();
        displayText_ = copy.join('\n');
        return;
    }
    const auto raw = [&](const QString &key) {
        return data.status.state == ReadState::Available ? data.values.value(key) : Attribute {data.status.state, {}, data.status.error};
    };
    const auto formatted = [&](const QString &key) {
        const Attribute value = raw(key);
        if (value.state != ReadState::Available) return propertyReadValue(value);
        if (key == "resolution") return QString(value.value).replace('x', QChar(0x00d7));
        if (key == "depth") return tr("%1 bits per pixel").arg(value.value);
        if (key == "refresh") return tr("%1 Hz").arg(QLocale().toString(value.value.toDouble(), 'f', 2));
        if (key == "rotation") return value.value == "0" ? tr("0°")
            : tr("%1° counterclockwise").arg(value.value);
        if (key == "reflection") return value.value == "xy" ? tr("X and Y axes") : value.value == "x" ? tr("X axis")
            : value.value == "y" ? tr("Y axis") : value.value == "vertical" ? tr("Vertical axis") : tr("None");
        if (key == "scale") {
            if (value.value == "custom") return tr("Custom transform");
            const QStringList axes = value.value.split(' ');
            const double x = axes.value(0).toDouble(), y = axes.value(1).toDouble();
            QString text = axes.value(0) == axes.value(1) ? tr("%1×").arg(QLocale().toString(x, 'g', 6))
                : tr("X: %1×, Y: %2×").arg(QLocale().toString(x, 'g', 6), QLocale().toString(y, 'g', 6));
            const QString kind = data.values.value("scale_kind").value;
            if (kind == "derived") text += tr(" (derived)");
            if (kind == "integer") text += tr(" (integer buffer scale)");
            return text;
        }
        return value.value;
    };
    auto *session = group(tr("Desktop session"));
    field(session, tr("Backend"), data.backend.isEmpty() ? tr("Unavailable") : data.backend);
    field(session, tr("Output"), data.output.isEmpty() ? tr("Unavailable") : data.output, device().path);
    field(session, tr("Configuration"), data.status.state == ReadState::Available
        ? (data.status.value == "inactive" ? tr("No active mode") : tr("Reported by desktop server"))
        : propertyReadValue(data.status), tr("Read-only session query. Unavailable can mean missing runtime libraries, "
            "an output not exposed by this session, or a protocol without usable output names."));
    auto *mode = group(tr("Display mode"));
    field(mode, tr("Resolution"), formatted("resolution"), data.sources.value("resolution"));
    field(mode, tr("Desktop color depth"), formatted("depth"), data.sources.value("depth"));
    field(mode, tr("Refresh rate"), formatted("refresh"), data.sources.value("refresh"));
    auto *transform = group(tr("Orientation and scale"));
    field(transform, tr("Rotation"), formatted("rotation"), data.sources.value("rotation"));
    field(transform, tr("Reflection"), formatted("reflection"), data.sources.value("reflection"));
    field(transform, tr("Output scale"), formatted("scale"), data.sources.value("scale"));
    int captionWidth = 0;
    for (const QLabel *caption : captions) captionWidth = qMax(captionWidth, caption->sizeHint().width());
    for (QGridLayout *form : forms) form->setColumnMinimumWidth(0, captionWidth);
    displayGroups_->addStretch();
    displayText_ = copy.join('\n');
}

void PropertiesDialog::rebuildUsb()
{
    const bool applicable = device().subsystem == "usb" && device().devtype == "usb_device";
    const int existing = tabs_->indexOf(usbPage_);
    if (applicable && existing < 0) tabs_->insertTab(2, usbPage_, tr("USB"));
    else if (!applicable && existing >= 0) {
        tabs_->removeTab(existing);
        usbPage_->hide();
    }
    while (QLayoutItem *item = usbGroups_->takeAt(0)) {
        delete item->widget();
        delete item;
    }
    usbText_.clear();
    if (!applicable) return;
    const UsbProperties &data = snapshot_.usb;
    QVector<QGridLayout *> forms;
    QVector<QLabel *> captions;
    QStringList copy;
    const auto group = [&](const QString &title) {
        auto *box = new QGroupBox(title, usbGroups_->parentWidget());
        auto *form = new QGridLayout(box);
        form->setColumnStretch(1, 1);
        forms.append(form);
        usbGroups_->addWidget(box);
        if (!copy.isEmpty()) copy.append(QString());
        copy.append(title);
        return form;
    };
    const auto field = [&](QGridLayout *form, const QString &caption, const QString &text, const QString &source = QString()) {
        auto *name = new QLabel(caption, form->parentWidget());
        name->setTextFormat(Qt::PlainText);
        captions.append(name);
        auto *value = plainLabel(text, form->parentWidget());
        QSizePolicy policy(QSizePolicy::Expanding, QSizePolicy::Preferred);
        policy.setHeightForWidth(true);
        value->setSizePolicy(policy);
        value->setAlignment(Qt::AlignLeading | Qt::AlignTop);
        value->setMinimumWidth(120);
        const QString hint = "<qt>" + source.toHtmlEscaped() + "</qt>";
        name->setToolTip(hint);
        value->setToolTip(hint);
        const int row = form->count() / 2;
        form->addWidget(name, row, 0, Qt::AlignLeading | Qt::AlignTop);
        form->addWidget(value, row, 1);
        copy.append(caption + ": " + text);
    };
    const auto value = [&](const QString &key) { return propertyReadValue(data.values.value(key)); };
    const auto source = [&](const QString &key) { return device().path + '/' + key; };
    auto *power = group(tr("Power declarations"));
    const Attribute attributes = data.values.value("bmAttributes");
    bool valid = false;
    const uint flags = attributes.value.toUInt(&valid, 16);
    QString supply = propertyReadValue(attributes), wake = supply;
    if (attributes.state == ReadState::Available) {
        if (!valid || flags > 255 || !(flags & 0x80)) {
            supply = wake = propertyReadValue({ReadState::Error, {}, EINVAL});
        } else {
            supply = flags & 0x40 ? tr("Self-powered") : tr("Bus-powered");
            wake = flags & 0x20 ? tr("Supported") : tr("Not supported");
        }
    }
    field(power, tr("Power source"), supply, source("bmAttributes"));
    field(power, tr("Declared maximum current"), value("bMaxPower"), source("bMaxPower"));
    field(power, tr("Remote wakeup capability"), wake, source("bmAttributes"));
    auto *configuration = group(tr("Configuration"));
    const Attribute active = data.values.value("bConfigurationValue");
    field(configuration, tr("Active configuration"), active.state == ReadState::Available && (active.value.isEmpty() || active.value == "0")
        ? tr("Unconfigured") : propertyReadValue(active), source("bConfigurationValue"));
    field(configuration, tr("Available configurations"), value("bNumConfigurations"), source("bNumConfigurations"));
    const Attribute description = data.values.value("configuration");
    field(configuration, tr("Description"), description.state == ReadState::Available && description.value.isEmpty()
        ? tr("Not provided") : propertyReadValue(description), source("configuration"));
    field(configuration, tr("Interface count"), value("bNumInterfaces"), source("bNumInterfaces"));
    // Class descriptions are deliberately limited; retain numeric codes for
    // vendor-specific and unfamiliar functions rather than guessing a name.
    const auto classText = [&](const Attribute &attribute) {
        if (attribute.state != ReadState::Available) return propertyReadValue(attribute);
        bool ok = false;
        const uint code = attribute.value.toUInt(&ok, 16);
        if (!ok || code > 255) return propertyReadValue({ReadState::Error, {}, EINVAL});
        QString name;
        switch (code) {
        case 0x01: name = tr("Audio"); break;
        case 0x02: name = tr("Communications"); break;
        case 0x03: name = tr("Human interface device"); break;
        case 0x07: name = tr("Printer"); break;
        case 0x08: name = tr("Mass storage"); break;
        case 0x09: name = tr("Hub"); break;
        case 0x0a: name = tr("CDC data"); break;
        case 0x0b: name = tr("Smart card"); break;
        case 0x0e: name = tr("Video"); break;
        case 0xe0: name = tr("Wireless controller"); break;
        case 0xef: name = tr("Miscellaneous"); break;
        case 0xfe: name = tr("Application-specific"); break;
        case 0xff: name = tr("Vendor-specific"); break;
        }
        const QString hex = QStringLiteral("0x%1").arg(code, 2, 16, QLatin1Char('0'));
        return name.isEmpty() ? hex : tr("%1 (%2)").arg(name, hex);
    };
    if (data.interfaceState.state != ReadState::Available || data.interfaces.isEmpty()) {
        auto *interfaces = group(tr("Interfaces"));
        field(interfaces, tr("Interfaces"), data.interfaceState.state == ReadState::Available
            ? tr("None observed") : propertyReadValue(data.interfaceState), device().path);
    }
    for (const UsbInterfaceProperties &interface : data.interfaces) {
        auto *form = group(tr("Interface %1").arg(interface.name));
        if (interface.values.contains("state")) {
            field(form, tr("Status"), propertyReadValue(interface.values.value("state")), interface.path);
            continue;
        }
        const auto entry = [&](const QString &key, const QString &caption) {
            field(form, caption, propertyReadValue(interface.values.value(key)), interface.path + '/' + key);
        };
        entry("bInterfaceNumber", tr("Number (hex)"));
        entry("bAlternateSetting", tr("Alternate setting"));
        const Attribute label = interface.values.value("interface");
        field(form, tr("Description"), label.state == ReadState::Available && label.value.isEmpty()
            ? tr("Not provided") : propertyReadValue(label), interface.path + "/interface");
        field(form, tr("Function class"), classText(interface.values.value("bInterfaceClass")), interface.path + "/bInterfaceClass");
        entry("bInterfaceSubClass", tr("Subclass (hex)"));
        entry("bInterfaceProtocol", tr("Protocol (hex)"));
        const Attribute driver = interface.values.value("driver");
        field(form, tr("Bound driver"), driver.state == ReadState::Available && driver.value.isEmpty()
            ? tr("Unbound") : propertyReadValue(driver), interface.path + "/driver");
    }
    int captionWidth = 0;
    for (const QLabel *caption : captions) captionWidth = qMax(captionWidth, caption->sizeHint().width());
    for (QGridLayout *form : forms) form->setColumnMinimumWidth(0, captionWidth);
    usbGroups_->addStretch();
    usbText_ = copy.join('\n');
}

void PropertiesDialog::rebuildStorage()
{
    for (const auto &page : {qMakePair(storagePage_, tr("Storage")),
                             qMakePair(volumesPage_, tr("Volumes")), qMakePair(healthPage_, tr("Health"))}) {
        const int existing = tabs_->indexOf(page.first);
        if (snapshot_.storage.applicable && existing < 0) tabs_->addTab(page.first, page.second);
        else if (!snapshot_.storage.applicable && existing >= 0) {
            tabs_->removeTab(existing);
            page.first->hide();
        }
    }
    const QString previousVolume = volumesTable_->currentRow() >= 0
        ? volumesTable_->item(volumesTable_->currentRow(), 0)->data(Qt::UserRole).toString() : QString();
    const QString previousDrive = healthDrive_->currentData().toString();
    storagePresentation_ = storagePresentation(snapshot_.storage, device().path);
    const QString scope = tr("Read-only metadata snapshot. Each entity is a separate layer; capacities are not added. "
        "Kernel and UDisks2 block records are merged only by their exact device-number mapping. "
        "Source paths remain in tooltips and copied text. Deep search includes native metadata only.");
    const auto hint = [](QLabel *label, const QString &text) {
        label->setToolTip("<qt>" + text.toHtmlEscaped().replace('\n', "<br>") + "</qt>");
    };
    hint(storageNotice_, scope + "\n\n" + storagePresentation_.notes);
    hint(volumesNotice_, scope + '\n' + tr("Related volumes may span several drives. Mount points cover only the application "
        "mount namespace. No filesystem is probed, mounted or unlocked.") + "\n\n" + storagePresentation_.notes);
    hint(healthNotice_, tr("Cached UDisks2 health only, separate from device operational status. "
        "Reload does not request SMART updates or self-tests. Missing evidence is unavailable; "
        "no reported warning does not guarantee health. Related drives are shown separately.")
        + "\n\n" + storagePresentation_.notes);
    showStorageFields(storage_, storagePresentation_.overview);
    {
        const QSignalBlocker blocker(volumesTable_);
        volumesTable_->setRowCount(0);
        int selected = -1;
        for (const StorageDisplayRow &entry : storagePresentation_.volumes) {
            const int row = volumesTable_->rowCount();
            volumesTable_->insertRow(row);
            const QStringList columns {entry.name, entry.kind, entry.capacity, entry.filesystem, entry.mounts};
            for (int column = 0; column < columns.size(); ++column) {
                auto *item = new QTableWidgetItem(columns[column]);
                item->setToolTip(entry.source.toHtmlEscaped());
                volumesTable_->setItem(row, column, item);
            }
            volumesTable_->item(row, 0)->setData(Qt::UserRole, entry.id);
            if (entry.id == previousVolume || (previousVolume.isEmpty() && entry.id == device().path)) selected = row;
        }
        if (selected < 0 && volumesTable_->rowCount()) selected = 0;
        volumesTable_->setCurrentCell(selected, 0);
    }
    showVolume();
    {
        const QSignalBlocker blocker(healthDrive_);
        healthDrive_->clear();
        for (const StorageDisplayRow &entry : storagePresentation_.health) {
            healthDrive_->addItem(entry.name, entry.id);
            healthDrive_->setItemData(healthDrive_->count() - 1, entry.source.toHtmlEscaped(), Qt::ToolTipRole);
        }
        const int selected = healthDrive_->findData(previousDrive);
        healthDrive_->setCurrentIndex(selected < 0 ? 0 : selected);
    }
    showHealth();
}
void PropertiesDialog::showVolume()
{
    const int row = volumesTable_->currentRow();
    if (row < 0 || row >= storagePresentation_.volumes.size()) {
        clearForm(volumeDetails_);
        volumeDetails_->addRow(plainLabel(tr("No related volume metadata available."), volumeDetails_->parentWidget()));
        return;
    }
    const StorageDisplayRow &entry = storagePresentation_.volumes[row];
    showStorageFields(volumeDetails_, entry.fields);
}
void PropertiesDialog::showHealth()
{
    const int row = healthDrive_->currentIndex();
    if (row < 0 || row >= storagePresentation_.health.size()) {
        clearForm(healthDetails_);
        healthDetails_->addRow(plainLabel(tr("Cached health unavailable."), healthDetails_->parentWidget()));
        return;
    }
    const StorageDisplayRow &entry = storagePresentation_.health[row];
    showStorageFields(healthDetails_, entry.fields);
}

void PropertiesDialog::updateEvents(const DeviceEventsSnapshot &history, const QString &monitorNote)
{
    Q_ASSERT(QThread::isMainThread());
    // A late removal observation may arrive after a property read detected removal.
    // Accept only the old token, retaining its rows even after global eviction.
    eventScope_ = tr("Live udev events since %1. Receipt times; exact device instance only. "
        "Latest %3 events per device; %2 retained globally. Historical logs are not queried.")
        .arg(history.started.isValid() ? history.started.toString(Qt::ISODate) : tr("monitor startup"))
        .arg(DeviceEventsSnapshot::Limit).arg(DeviceEventsSnapshot::PerDeviceLimit);
    if (history.evicted) eventScope_ += '\n' + tr("%1 older events evicted from the global history.").arg(history.evicted);
    if (history.unassociated) eventScope_ += '\n' + tr("%1 events could not be associated safely with an inventory instance.").arg(history.unassociated);
    if (history.gaps) eventScope_ += '\n' + tr("%1 event-identity resets after monitor failures/losses; coverage is incomplete.").arg(history.gaps);
    if (!history.monitoring) eventScope_ += '\n' + tr("Live event monitoring is unavailable. Viewing and retained events remain available.");
    if (!monitorNote.isEmpty()) eventScope_ += '\n' + monitorNote;
    if (!device().eventInstance) eventScope_ += '\n' + tr("No safe live-event association is available for this instance.");
    if (device().subsystem == "efivarfs") eventScope_ += '\n' + tr("EFI variables are reconciled by inventory scans; libudev does not supply their value-change history.");
    QVector<DeviceEvent> next;
    for (auto it = history.records.crbegin(); it != history.records.crend(); ++it) {
        if (!device().eventInstance || it->instance != device().eventInstance || it->path != device().path) continue;
        next.append(*it);
        if (next.size() == DeviceEventsSnapshot::PerDeviceLimit) break;
    }
    if (removed_) {
        for (const DeviceEvent &old : displayedEvents_) {
            bool retained = false;
            for (const DeviceEvent &event : next)
                if (event.sequence == old.sequence && event.elapsedMs == old.elapsedMs
                    && event.observed == old.observed && event.action == old.action) { retained = true; break; }
            if (!retained && next.size() < DeviceEventsSnapshot::PerDeviceLimit) next.append(old);
        }
        eventScope_ += '\n' + tr("Removed/replaced device: retained events are a read-only instance snapshot.");
    }
    if (next.isEmpty()) eventScope_ += '\n' + tr("No retained live events for this instance. This does not establish that the device has had no errors.");
    eventsNotice_->setToolTip("<qt>" + eventScope_.toHtmlEscaped().replace('\n', "<br>") + "</qt>");
    bool unchanged = next.size() == displayedEvents_.size();
    for (int i = 0; unchanged && i < next.size(); ++i) {
        const DeviceEvent &a = next.at(i);
        const DeviceEvent &b = displayedEvents_.at(i);
        unchanged = a.sequence == b.sequence && a.elapsedMs == b.elapsedMs && a.observed == b.observed
            && a.action == b.action && a.path == b.path && a.instance == b.instance;
    }
    if (unchanged) return; // Unrelated events must not reset selection/scroll.
    const int oldRow = eventsTable_->currentRow();
    DeviceEvent selected;
    const bool hadSelection = oldRow >= 0 && oldRow < displayedEvents_.size();
    if (hadSelection) selected = displayedEvents_.at(oldRow);
    displayedEvents_ = std::move(next);
    const QSignalBlocker blocker(eventsTable_);
    eventsTable_->setRowCount(0);
    eventsTable_->setRowCount(displayedEvents_.size());
    int restore = -1;
    for (int row = 0; row < displayedEvents_.size(); ++row) {
        const DeviceEvent &event = displayedEvents_.at(row);
        const QStringList values {event.observed.toString(Qt::ISODateWithMs), event.action, eventDescription(event)};
        for (int column = 0; column < values.size(); ++column)
            eventsTable_->setItem(row, column, new QTableWidgetItem(values[column]));
        if (hadSelection && event.sequence == selected.sequence && event.elapsedMs == selected.elapsedMs
            && event.action == selected.action) restore = row;
    }
    if (restore < 0 && !displayedEvents_.isEmpty()) restore = 0;
    if (restore >= 0) eventsTable_->setCurrentCell(restore, 0);
    showEvent();
}
void PropertiesDialog::showEvent()
{
    const int row = eventsTable_->currentRow();
    eventDetails_->setPlainText(row >= 0 && row < displayedEvents_.size() ? eventText(displayedEvents_.at(row)) : QString());
}
void PropertiesDialog::copyEvents(bool selectedOnly)
{
    QStringList text {eventScope_};
    if (selectedOnly) {
        QList<int> rows;
        for (const QModelIndex &index : eventsTable_->selectionModel()->selectedRows()) rows.append(index.row());
        std::sort(rows.begin(), rows.end());
        for (const int row : rows) text.append(eventText(displayedEvents_.at(row)));
    } else {
        for (const DeviceEvent &event : displayedEvents_) text.append(eventText(event));
    }
    QApplication::clipboard()->setText(text.join("\n\n"));
}
