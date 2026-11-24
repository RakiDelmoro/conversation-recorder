#ifndef AIVAD_MODEL_H
#define AIVAD_MODEL_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct AivadModel AivadModel;

/* Allocate a model instance. Returns 0 on success. */
int aivad_create(AivadModel** out);

/* Release a model instance. */
void aivad_destroy(AivadModel* model);

/* Clear the recurrent state. */
int aivad_reset(AivadModel* model);

/*
 * Run one 16 ms frame. `features` is the 3 x 41 context window produced by
 * TEN VAD's front end, in the same order as aed.c. `out` receives [0,1].
 */
int aivad_process(AivadModel* model, const float* features, float* out);

#ifdef __cplusplus
}
#endif

#endif /* AIVAD_MODEL_H */
