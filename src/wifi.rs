//! Station-mode Wi-Fi bring-up.

use anyhow::{bail, Result};
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration, PmfConfiguration};
use esp_idf_hal::modem::Modem;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::netif::{EspNetif, NetifStack};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{EspWifi, WifiDriver};

use crate::config;

pub fn setup(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<EspWifi<'static>> {
    if config::WIFI_SSID.is_empty() {
        bail!("set WIFI_SSID in src/config.rs");
    }

    let ssid = heapless::String::<32>::try_from(config::WIFI_SSID)
        .map_err(|_| anyhow::anyhow!("WIFI_SSID is longer than 32 bytes"))?;
    let password = heapless::String::<64>::try_from(config::WIFI_PASSWORD)
        .map_err(|_| anyhow::anyhow!("WIFI_PASSWORD is longer than 64 bytes"))?;

    let sta = EspNetif::new(NetifStack::Sta)?;
    let ap = EspNetif::new(NetifStack::Ap)?;
    let mut wifi = EspWifi::wrap_all(WifiDriver::new(modem, sysloop, Some(nvs))?, sta, ap)?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid,
        password,
        auth_method: AuthMethod::WPA2Personal,
        pmf_cfg: PmfConfiguration::NotCapable,
        ..Default::default()
    }))?;

    wifi.start()?;
    wifi.connect()?;

    unsafe {
        esp_idf_svc::sys::esp_wifi_set_ps(
            esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_MIN_MODEM,
        );
    }

    Ok(wifi)
}
