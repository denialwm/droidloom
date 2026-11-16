/* Check real GPU IDs against Mesa's actual model lookup, without a device. */
#include <assert.h>
#include <string.h>
#include "panfrost/model/pan_model.h"

int main(void)
{
    const struct pan_model *model = pan_get_model(0xb8a31030, 4, 6, 0);
    assert(model != NULL && strcmp(model->name, "Mali-G615") == 0);
    assert(pan_arch(0xb8a31030) == 11);
    assert(model->tilebuffer.color_size == 32768);
    assert(model->tilebuffer.z_size == 16384);
    model = pan_get_model(0xb8a20000, 4, 7, 0);
    assert(model != NULL && strcmp(model->name, "Mali-G715") == 0);
    model = pan_get_model(0xa8070000, 0, 4, 0);
    assert(model != NULL && strcmp(model->name, "Mali-G610") == 0);
    assert(pan_get_model(0xb8a31030, 0, 6, 0) == NULL);
    assert(pan_get_model(0xdeadbeef, 4, 6, 0) == NULL);
    return 0;
}
