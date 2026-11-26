#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

int vadnet_bridge_init(void);
int vadnet_bridge_push(const int16_t *samples, int count);
void vadnet_bridge_destroy(void);

#ifdef __cplusplus
}
#endif
