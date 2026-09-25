#include "AppSettings.h"

AppSettings::AppSettings(QObject *parent) : QObject(parent) { }

void AppSettings::setThemeOverride(ThemeOverride override)
{
    if (m_themeOverride == override) {
        return;
    }
    m_themeOverride = override;
    Q_EMIT themeOverrideChanged();
}
