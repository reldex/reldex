#pragma once

// `AppSettings` -- M3.1: the live-editable half of "every default is
// user-configurable" (owner decision, docs/exec-plans/active/phase-1.md
// "Owner decisions") for exactly one setting so far: the light/dark theme
// override `ui/app/Theme.qml` resolves against the live system colour
// scheme. It is a plain in-memory QObject property, not a settings *store*
// -- there is deliberately no persistence here, and none is added by this
// task.
//
// TODO(M2.9/M3.6): once the settings model (`db-core` workspace/settings,
// three-level application/profile/worksheet resolution, M2.9) and its UI
// (M3.6) exist, `themeOverride` must read its default from, and write
// changes back through, that model instead of living only in this object's
// process memory. This class should then become a thin QML-facing
// projection of that model's "theme" setting rather than the setting's only
// storage. Nothing about `Theme.qml`'s binding shape needs to change for
// that -- it only reads this property and reacts to `themeOverrideChanged`.
//
// Kept in `ui/adapter` (not `ui/app`) per the task brief: it is a small
// adapter-owned QObject, not a QML file, and future settings belong beside
// it rather than scattered across QML singletons.

#include <QObject>
#include <QtQml/qqmlregistration.h>

class AppSettings : public QObject
{
    Q_OBJECT
    QML_ELEMENT
    QML_SINGLETON

    /// System (default: follow `Application.styleHints.colorScheme`), Light,
    /// or Dark. Changing this takes effect immediately through ordinary QML
    /// property bindings in `Theme.qml` -- no window or engine recreation,
    /// which is the acceptance criterion in
    /// `docs/exec-plans/active/phase-1.md` row M3.1 ("theme switch has no
    /// restart").
    Q_PROPERTY(ThemeOverride themeOverride READ themeOverride WRITE setThemeOverride NOTIFY
                       themeOverrideChanged)

public:
    enum ThemeOverride { System, Light, Dark };
    Q_ENUM(ThemeOverride)

    explicit AppSettings(QObject *parent = nullptr);

    [[nodiscard]] ThemeOverride themeOverride() const noexcept { return m_themeOverride; }
    void setThemeOverride(ThemeOverride override);

Q_SIGNALS:
    void themeOverrideChanged();

private:
    ThemeOverride m_themeOverride = System;
};
