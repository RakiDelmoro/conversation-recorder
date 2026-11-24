// Inference for the TEN VAD model without ONNX Runtime.
// Weights are generated from ten-vad.onnx by tools/export_ten_vad_weights.py.
// TEN VAD is Copyright (c) 2025 Agora, Apache-2.0 with additional conditions.

#include "aivad_model.h"

#include <math.h>
#include <stdlib.h>
#include <string.h>

#include "weights.h"

#define HIDDEN 64
#define FEAT_LEN 41
#define CONTEXT 3
#define IN_DIM80 80

struct AivadModel {
  float h1[HIDDEN];
  float c1[HIDDEN];
  float h2[HIDDEN];
  float c2[HIDDEN];

  float x0[39];
  float x1[16 * 39];
  float pool[16 * 19];
  float d2[16 * 10];
  float p2[16 * 10];
  float d3[16 * 5];
  float p3[16 * 5];
  float flat[IN_DIM80];
  float gates[4 * HIDDEN];
  float z[128];
  float dense[32];
};

static float sigmoidf(float x) { return 1.0f / (1.0f + expf(-x)); }

static void relu_inplace(float* x, int n) {
  for (int i = 0; i < n; ++i) {
    if (x[i] < 0.0f) {
      x[i] = 0.0f;
    }
  }
}

static void lstm_step(const float* x, int in_dim, const float* w,
                      const float* r, const float* b, float* h, float* c,
                      float* gates) {
  for (int row = 0; row < 4 * HIDDEN; ++row) {
    float s = b[row] + b[4 * HIDDEN + row];
    const float* wr = w + (size_t)row * in_dim;
    for (int i = 0; i < in_dim; ++i) {
      s += wr[i] * x[i];
    }
    const float* rr = r + (size_t)row * HIDDEN;
    for (int i = 0; i < HIDDEN; ++i) {
      s += rr[i] * h[i];
    }
    gates[row] = s;
  }

  for (int j = 0; j < HIDDEN; ++j) {
    const float i = sigmoidf(gates[j]);
    const float o = sigmoidf(gates[HIDDEN + j]);
    const float f = sigmoidf(gates[2 * HIDDEN + j]);
    const float g = tanhf(gates[3 * HIDDEN + j]);
    c[j] = f * c[j] + i * g;
    h[j] = o * tanhf(c[j]);
  }
}

int aivad_create(AivadModel** out) {
  if (out == NULL) {
    return -1;
  }
  AivadModel* m = (AivadModel*)calloc(1, sizeof(AivadModel));
  if (m == NULL) {
    return -1;
  }
  *out = m;
  return 0;
}

void aivad_destroy(AivadModel* model) { free(model); }

int aivad_reset(AivadModel* model) {
  if (model == NULL) {
    return -1;
  }
  memset(model->h1, 0, sizeof(model->h1));
  memset(model->c1, 0, sizeof(model->c1));
  memset(model->h2, 0, sizeof(model->h2));
  memset(model->c2, 0, sizeof(model->c2));
  return 0;
}

int aivad_process(AivadModel* model, const float* features, float* out) {
  if (model == NULL || features == NULL || out == NULL) {
    return -1;
  }

  // 3x3 convolution over the 3 x 41 context window, valid padding.
  for (int w = 0; w < 39; ++w) {
    float s = 0.0f;
    for (int kh = 0; kh < 3; ++kh) {
      for (int kw = 0; kw < 3; ++kw) {
        s += features[kh * FEAT_LEN + w + kw] * W_CONV0[kh * 3 + kw];
      }
    }
    model->x0[w] = s;
  }

  // Pointwise expansion 1 -> 16, bias, ReLU.
  for (int c = 0; c < 16; ++c) {
    for (int w = 0; w < 39; ++w) {
      model->x1[c * 39 + w] = B_CONV1[c] + W_CONV1[c] * model->x0[w];
    }
  }
  relu_inplace(model->x1, 16 * 39);

  // MaxPool kernel [1,3], stride [1,2].
  for (int c = 0; c < 16; ++c) {
    for (int w = 0; w < 19; ++w) {
      const float* p = &model->x1[c * 39 + 2 * w];
      float m = p[0];
      if (p[1] > m) m = p[1];
      if (p[2] > m) m = p[2];
      model->pool[c * 19 + w] = m;
    }
  }

  // Depthwise 1x3, group 16, stride 2, pad left 1, right 1.
  for (int c = 0; c < 16; ++c) {
    for (int w = 0; w < 10; ++w) {
      float s = 0.0f;
      for (int kw = 0; kw < 3; ++kw) {
        const int idx = 2 * w + kw - 1;
        if (idx >= 0 && idx < 19) {
          s += model->pool[c * 19 + idx] * W_DW2[c * 3 + kw];
        }
      }
      model->d2[c * 10 + w] = s;
    }
  }

  // Pointwise 16 -> 16, bias, ReLU.
  for (int oc = 0; oc < 16; ++oc) {
    for (int w = 0; w < 10; ++w) {
      float s = B_PW2[oc];
      for (int ic = 0; ic < 16; ++ic) {
        s += model->d2[ic * 10 + w] * W_PW2[oc * 16 + ic];
      }
      model->p2[oc * 10 + w] = s;
    }
  }
  relu_inplace(model->p2, 16 * 10);

  // Depthwise 1x3, group 16, stride 2, pad left 0, right 1.
  for (int c = 0; c < 16; ++c) {
    for (int w = 0; w < 5; ++w) {
      float s = 0.0f;
      for (int kw = 0; kw < 3; ++kw) {
        const int idx = 2 * w + kw;
        if (idx < 10) {
          s += model->p2[c * 10 + idx] * W_DW3[c * 3 + kw];
        }
      }
      model->d3[c * 5 + w] = s;
    }
  }

  // Pointwise 16 -> 16, bias, ReLU.
  for (int oc = 0; oc < 16; ++oc) {
    for (int w = 0; w < 5; ++w) {
      float s = B_PW3[oc];
      for (int ic = 0; ic < 16; ++ic) {
        s += model->d3[ic * 5 + w] * W_PW3[oc * 16 + ic];
      }
      model->p3[oc * 5 + w] = s;
    }
  }
  relu_inplace(model->p3, 16 * 5);

  // Transpose to [5,16] and flatten.
  for (int w = 0; w < 5; ++w) {
    for (int c = 0; c < 16; ++c) {
      model->flat[w * 16 + c] = model->p3[c * 5 + w];
    }
  }

  lstm_step(model->flat, 80, W_LSTM1, R_LSTM1, B_LSTM1, model->h1, model->c1,
            model->gates);
  lstm_step(model->h1, 64, W_LSTM2, R_LSTM2, B_LSTM2, model->h2, model->c2,
            model->gates);

  for (int i = 0; i < 64; ++i) {
    model->z[i] = model->h2[i];
    model->z[64 + i] = model->h1[i];
  }

  for (int k = 0; k < 32; ++k) {
    float s = B_DENSE3[k];
    for (int j = 0; j < 128; ++j) {
      s += model->z[j] * W_DENSE3[j * 32 + k];
    }
    model->dense[k] = s > 0.0f ? s : 0.0f;
  }

  float s = B_DENSE5[0];
  for (int j = 0; j < 32; ++j) {
    s += model->dense[j] * W_DENSE5[j];
  }
  *out = sigmoidf(s);
  return 0;
}
