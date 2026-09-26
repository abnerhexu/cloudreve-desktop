#include <stdint.h>
typedef struct Operation Operation;
Operation *crfp_create(const char *input);
void crfp_cancel(Operation *operation);
char *crfp_run(Operation *operation);
void crfp_free(Operation *operation);
void crfp_string_free(char *value);

uint64_t crfp_progress(Operation *operation);
