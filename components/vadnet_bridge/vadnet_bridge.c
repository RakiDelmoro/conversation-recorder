#include "vadnet_bridge.h"

#include <string.h>

#include "esp_log.h"
#include "esp_vadn_models.h"
#include "model_path.h"

static const char *TAG = "vadnet";

static srmodel_list_t *s_models;
static const esp_vadn_iface_t *s_iface;
static model_iface_data_t *s_vad;
static int s_chunk;
static int s_fill;
static int s_last;
static int16_t s_buf[2048];

int vadnet_bridge_init(void)
{
    s_models = esp_srmodel_init("model");
    if (s_models == NULL) {
        ESP_LOGE(TAG, "model partition not found");
        return -1;
    }

    char *name = esp_srmodel_filter(s_models, ESP_VADN_PREFIX, NULL);
    if (name == NULL) {
        ESP_LOGE(TAG, "no VADNet model in partition");
        esp_srmodel_deinit(s_models);
        s_models = NULL;
        return -2;
    }

    s_iface = esp_vadn_handle_from_name(name);
    if (s_iface == NULL) {
        ESP_LOGE(TAG, "VADNet iface for %s not found", name);
        esp_srmodel_deinit(s_models);
        s_models = NULL;
        return -3;
    }

    s_vad = s_iface->create(name, VAD_MODE_2, 1, 250, 1000);
    if (s_vad == NULL) {
        ESP_LOGE(TAG, "VADNet create failed for %s", name);
        esp_srmodel_deinit(s_models);
        s_models = NULL;
        return -4;
    }

    s_chunk = s_iface->get_samp_chunksize(s_vad);
    if (s_chunk <= 0 || s_chunk > (int)(sizeof(s_buf) / sizeof(s_buf[0]))) {
        ESP_LOGE(TAG, "invalid VADNet chunk size %d", s_chunk);
        s_iface->destroy(s_vad);
        s_vad = NULL;
        esp_srmodel_deinit(s_models);
        s_models = NULL;
        return -5;
    }

    s_fill = 0;
    s_last = 0;
    s_iface->set_energy_threshold(s_vad, -45.0f);
    if (!s_iface->set_det_threshold(s_vad, 0.5f)) {
        ESP_LOGW(TAG, "could not set VADNet detection threshold");
    }
    ESP_LOGI(TAG, "VADNet %s ready: chunk=%d rate=%d threshold=%.3f energy=%.1f dBFS",
             name, s_chunk, s_iface->get_samp_rate(s_vad),
             s_iface->get_det_threshold(s_vad),
             s_iface->get_energy_threshold(s_vad));
    return 0;
}

int vadnet_bridge_push(const int16_t *samples, int count)
{
    if (s_vad == NULL || samples == NULL || count <= 0) {
        return s_last;
    }

    int offset = 0;
    while (offset < count) {
        int space = s_chunk - s_fill;
        int take = count - offset;
        if (take > space) {
            take = space;
        }
        memcpy(&s_buf[s_fill], &samples[offset], (size_t)take * sizeof(int16_t));
        s_fill += take;
        offset += take;

        if (s_fill == s_chunk) {
            int speech = s_iface->detect(s_vad, s_buf) == VAD_SPEECH;
            if (speech != s_last) {
                ESP_LOGI(TAG, "VAD %s", speech ? "speech" : "silence");
            }
            s_last = speech;
            s_fill = 0;
        }
    }
    return s_last;
}

void vadnet_bridge_destroy(void)
{
    if (s_vad != NULL) {
        memset(s_buf, 0, sizeof(s_buf));
        s_fill = 0;
        s_iface->destroy(s_vad);
        s_vad = NULL;
    }
    if (s_models != NULL) {
        esp_srmodel_deinit(s_models);
        s_models = NULL;
    }
}
