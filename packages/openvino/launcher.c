#define _POSIX_C_SOURCE 200809L

#include <dirent.h>
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define INVENTORY_NAME "package-inventory.v1"
#define LAUNCHER_NAME "cfetch-openvino-adapter"
#define RUNTIME_NAME "cfetch-openvino-adapter-runtime"
#define HEADER "cfetch-package-inventory-v1"
#define MAX_FILES 50000U
#define MAX_LINE 8192U
#define MAX_TOTAL_BYTES (UINT64_C(4) * 1024U * 1024U * 1024U)

/* Volatile prevents the compiler from folding the unpatched-placeholder
 * guard into an unconditional exit.  Assembly patches these exact 64 bytes. */
static volatile char expected_inventory_sha256[] =
    "0000000000000000000000000000000000000000000000000000000000000000";

typedef struct {
    uint32_t state[8];
    uint64_t bit_count;
    uint8_t buffer[64];
    size_t buffer_length;
} sha256_context;

typedef struct {
    char *path;
    uint8_t digest[32];
    uint64_t bytes;
    int executable;
    int seen;
} inventory_entry;

static const uint32_t sha256_constants[64] = {
    0x428a2f98U, 0x71374491U, 0xb5c0fbcfU, 0xe9b5dba5U,
    0x3956c25bU, 0x59f111f1U, 0x923f82a4U, 0xab1c5ed5U,
    0xd807aa98U, 0x12835b01U, 0x243185beU, 0x550c7dc3U,
    0x72be5d74U, 0x80deb1feU, 0x9bdc06a7U, 0xc19bf174U,
    0xe49b69c1U, 0xefbe4786U, 0x0fc19dc6U, 0x240ca1ccU,
    0x2de92c6fU, 0x4a7484aaU, 0x5cb0a9dcU, 0x76f988daU,
    0x983e5152U, 0xa831c66dU, 0xb00327c8U, 0xbf597fc7U,
    0xc6e00bf3U, 0xd5a79147U, 0x06ca6351U, 0x14292967U,
    0x27b70a85U, 0x2e1b2138U, 0x4d2c6dfcU, 0x53380d13U,
    0x650a7354U, 0x766a0abbU, 0x81c2c92eU, 0x92722c85U,
    0xa2bfe8a1U, 0xa81a664bU, 0xc24b8b70U, 0xc76c51a3U,
    0xd192e819U, 0xd6990624U, 0xf40e3585U, 0x106aa070U,
    0x19a4c116U, 0x1e376c08U, 0x2748774cU, 0x34b0bcb5U,
    0x391c0cb3U, 0x4ed8aa4aU, 0x5b9cca4fU, 0x682e6ff3U,
    0x748f82eeU, 0x78a5636fU, 0x84c87814U, 0x8cc70208U,
    0x90befffaU, 0xa4506cebU, 0xbef9a3f7U, 0xc67178f2U,
};

static uint32_t rotate_right(uint32_t value, unsigned int bits) {
    return (value >> bits) | (value << (32U - bits));
}

static void sha256_transform(sha256_context *context, const uint8_t block[64]) {
    uint32_t words[64];
    uint32_t a, b, c, d, e, f, g, h;
    size_t index;
    for (index = 0; index < 16; ++index) {
        size_t offset = index * 4;
        words[index] = ((uint32_t)block[offset] << 24)
                     | ((uint32_t)block[offset + 1] << 16)
                     | ((uint32_t)block[offset + 2] << 8)
                     | (uint32_t)block[offset + 3];
    }
    for (index = 16; index < 64; ++index) {
        uint32_t s0 = rotate_right(words[index - 15], 7)
                    ^ rotate_right(words[index - 15], 18)
                    ^ (words[index - 15] >> 3);
        uint32_t s1 = rotate_right(words[index - 2], 17)
                    ^ rotate_right(words[index - 2], 19)
                    ^ (words[index - 2] >> 10);
        words[index] = words[index - 16] + s0 + words[index - 7] + s1;
    }
    a = context->state[0]; b = context->state[1];
    c = context->state[2]; d = context->state[3];
    e = context->state[4]; f = context->state[5];
    g = context->state[6]; h = context->state[7];
    for (index = 0; index < 64; ++index) {
        uint32_t sum1 = rotate_right(e, 6) ^ rotate_right(e, 11) ^ rotate_right(e, 25);
        uint32_t choice = (e & f) ^ ((~e) & g);
        uint32_t temporary1 = h + sum1 + choice + sha256_constants[index] + words[index];
        uint32_t sum0 = rotate_right(a, 2) ^ rotate_right(a, 13) ^ rotate_right(a, 22);
        uint32_t majority = (a & b) ^ (a & c) ^ (b & c);
        uint32_t temporary2 = sum0 + majority;
        h = g; g = f; f = e; e = d + temporary1;
        d = c; c = b; b = a; a = temporary1 + temporary2;
    }
    context->state[0] += a; context->state[1] += b;
    context->state[2] += c; context->state[3] += d;
    context->state[4] += e; context->state[5] += f;
    context->state[6] += g; context->state[7] += h;
}

static void sha256_init(sha256_context *context) {
    static const uint32_t initial[8] = {
        0x6a09e667U, 0xbb67ae85U, 0x3c6ef372U, 0xa54ff53aU,
        0x510e527fU, 0x9b05688cU, 0x1f83d9abU, 0x5be0cd19U,
    };
    memcpy(context->state, initial, sizeof(initial));
    context->bit_count = 0;
    context->buffer_length = 0;
}

static void sha256_update(sha256_context *context, const uint8_t *data, size_t length) {
    while (length > 0) {
        size_t available = 64 - context->buffer_length;
        size_t take = length < available ? length : available;
        memcpy(context->buffer + context->buffer_length, data, take);
        context->buffer_length += take;
        context->bit_count += (uint64_t)take * 8U;
        data += take;
        length -= take;
        if (context->buffer_length == 64) {
            sha256_transform(context, context->buffer);
            context->buffer_length = 0;
        }
    }
}

static void sha256_final(sha256_context *context, uint8_t digest[32]) {
    size_t index;
    uint64_t bits = context->bit_count;
    context->buffer[context->buffer_length++] = 0x80U;
    if (context->buffer_length > 56) {
        while (context->buffer_length < 64)
            context->buffer[context->buffer_length++] = 0;
        sha256_transform(context, context->buffer);
        context->buffer_length = 0;
    }
    while (context->buffer_length < 56)
        context->buffer[context->buffer_length++] = 0;
    for (index = 0; index < 8; ++index)
        context->buffer[56 + index] = (uint8_t)(bits >> (56U - 8U * index));
    sha256_transform(context, context->buffer);
    for (index = 0; index < 8; ++index) {
        digest[index * 4] = (uint8_t)(context->state[index] >> 24);
        digest[index * 4 + 1] = (uint8_t)(context->state[index] >> 16);
        digest[index * 4 + 2] = (uint8_t)(context->state[index] >> 8);
        digest[index * 4 + 3] = (uint8_t)context->state[index];
    }
}

static int hash_file(const char *path, uint8_t digest[32]) {
    FILE *input = fopen(path, "rb");
    uint8_t buffer[64 * 1024];
    sha256_context context;
    if (input == NULL)
        return -1;
    sha256_init(&context);
    while (!feof(input)) {
        size_t count = fread(buffer, 1, sizeof(buffer), input);
        if (count > 0)
            sha256_update(&context, buffer, count);
        if (ferror(input)) {
            fclose(input);
            return -1;
        }
    }
    if (fclose(input) != 0)
        return -1;
    sha256_final(&context, digest);
    return 0;
}

static int hex_value(char character) {
    if (character >= '0' && character <= '9') return character - '0';
    if (character >= 'a' && character <= 'f') return character - 'a' + 10;
    return -1;
}

static int parse_digest(const char *text, uint8_t digest[32]) {
    size_t index;
    if (strlen(text) != 64)
        return -1;
    for (index = 0; index < 32; ++index) {
        int high = hex_value(text[index * 2]);
        int low = hex_value(text[index * 2 + 1]);
        if (high < 0 || low < 0)
            return -1;
        digest[index] = (uint8_t)((high << 4) | low);
    }
    return 0;
}

static void digest_hex(const uint8_t digest[32], char output[65]) {
    static const char digits[] = "0123456789abcdef";
    size_t index;
    for (index = 0; index < 32; ++index) {
        output[index * 2] = digits[digest[index] >> 4];
        output[index * 2 + 1] = digits[digest[index] & 15U];
    }
    output[64] = '\0';
}

static int safe_relative_path(const char *path) {
    const char *component = path;
    const char *cursor;
    if (*path == '\0' || *path == '/' || strlen(path) >= PATH_MAX)
        return 0;
    for (cursor = path; ; ++cursor) {
        if (*cursor == '\\' || *cursor == '\t' || *cursor == '\r' || *cursor == '\n')
            return 0;
        if (*cursor == '/' || *cursor == '\0') {
            size_t length = (size_t)(cursor - component);
            if (length == 0 || (length == 1 && component[0] == '.')
                || (length == 2 && component[0] == '.' && component[1] == '.'))
                return 0;
            if (*cursor == '\0')
                break;
            component = cursor + 1;
        }
    }
    return strcmp(path, INVENTORY_NAME) != 0 && strcmp(path, LAUNCHER_NAME) != 0;
}

static void free_entries(inventory_entry *entries, size_t count) {
    size_t index;
    for (index = 0; index < count; ++index)
        free(entries[index].path);
    free(entries);
}

static int load_inventory(const char *path, inventory_entry **output, size_t *output_count) {
    FILE *input = fopen(path, "r");
    inventory_entry *entries = NULL;
    size_t count = 0, capacity = 0;
    uint64_t total = 0;
    char *line = NULL;
    size_t line_capacity = 0;
    ssize_t length;
    if (input == NULL)
        return -1;
    length = getline(&line, &line_capacity, input);
    if (length <= 0 || strcmp(line, HEADER "\n") != 0)
        goto fail;
    while ((length = getline(&line, &line_capacity, input)) >= 0) {
        char *digest_text, *size_text, *mode_text, *relative;
        char *tab1, *tab2, *tab3, *end;
        unsigned long long bytes;
        inventory_entry *entry;
        if ((size_t)length > MAX_LINE || length < 1 || line[length - 1] != '\n')
            goto fail;
        line[length - 1] = '\0';
        digest_text = line;
        tab1 = strchr(digest_text, '\t');
        if (tab1 == NULL) goto fail;
        *tab1 = '\0'; size_text = tab1 + 1;
        tab2 = strchr(size_text, '\t');
        if (tab2 == NULL) goto fail;
        *tab2 = '\0'; mode_text = tab2 + 1;
        tab3 = strchr(mode_text, '\t');
        if (tab3 == NULL) goto fail;
        *tab3 = '\0'; relative = tab3 + 1;
        if (strchr(relative, '\t') != NULL || !safe_relative_path(relative))
            goto fail;
        if (count > 0 && strcmp(entries[count - 1].path, relative) >= 0)
            goto fail;
        if (*size_text == '\0' || *size_text == '0')
            goto fail;
        errno = 0;
        bytes = strtoull(size_text, &end, 10);
        if (errno != 0 || *end != '\0' || bytes == 0 || bytes > MAX_TOTAL_BYTES)
            goto fail;
        if (strcmp(mode_text, "0") != 0 && strcmp(mode_text, "1") != 0)
            goto fail;
        if (count == MAX_FILES || total > MAX_TOTAL_BYTES - (uint64_t)bytes)
            goto fail;
        if (count == capacity) {
            size_t next = capacity == 0 ? 128 : capacity * 2;
            inventory_entry *grown = realloc(entries, next * sizeof(*entries));
            if (grown == NULL)
                goto fail;
            entries = grown;
            capacity = next;
        }
        entry = &entries[count];
        memset(entry, 0, sizeof(*entry));
        if (parse_digest(digest_text, entry->digest) != 0)
            goto fail;
        entry->path = strdup(relative);
        if (entry->path == NULL)
            goto fail;
        entry->bytes = (uint64_t)bytes;
        entry->executable = mode_text[0] == '1';
        total += entry->bytes;
        ++count;
    }
    if (ferror(input) || count == 0)
        goto fail;
    free(line);
    if (fclose(input) != 0) {
        free_entries(entries, count);
        return -1;
    }
    *output = entries;
    *output_count = count;
    return 0;
fail:
    free(line);
    fclose(input);
    free_entries(entries, count);
    return -1;
}

static inventory_entry *find_entry(inventory_entry *entries, size_t count, const char *path) {
    size_t low = 0, high = count;
    while (low < high) {
        size_t middle = low + (high - low) / 2;
        int comparison = strcmp(entries[middle].path, path);
        if (comparison == 0)
            return &entries[middle];
        if (comparison < 0)
            low = middle + 1;
        else
            high = middle;
    }
    return NULL;
}

static int verify_regular_file(const char *full, const struct stat *metadata,
                               inventory_entry *entry) {
    uint8_t digest[32];
    if (entry == NULL || entry->seen || (uint64_t)metadata->st_size != entry->bytes
        || ((metadata->st_mode & 0111) != 0) != entry->executable)
        return -1;
    if (hash_file(full, digest) != 0 || memcmp(digest, entry->digest, 32) != 0)
        return -1;
    entry->seen = 1;
    return 0;
}

static int scan_directory(const char *root, const char *relative,
                          inventory_entry *entries, size_t count) {
    char directory_path[PATH_MAX];
    DIR *directory;
    struct dirent *item;
    if (*relative == '\0')
        snprintf(directory_path, sizeof(directory_path), "%s", root);
    else if (snprintf(directory_path, sizeof(directory_path), "%s/%s", root, relative)
             >= (int)sizeof(directory_path))
        return -1;
    directory = opendir(directory_path);
    if (directory == NULL)
        return -1;
    while ((item = readdir(directory)) != NULL) {
        char child_relative[PATH_MAX], child_full[PATH_MAX];
        struct stat metadata;
        if (strcmp(item->d_name, ".") == 0 || strcmp(item->d_name, "..") == 0)
            continue;
        if (strchr(item->d_name, '\t') != NULL || strchr(item->d_name, '\n') != NULL
            || strchr(item->d_name, '\r') != NULL || strchr(item->d_name, '\\') != NULL)
            goto fail;
        if (*relative == '\0') {
            if (snprintf(child_relative, sizeof(child_relative), "%s", item->d_name)
                >= (int)sizeof(child_relative)) goto fail;
        } else if (snprintf(child_relative, sizeof(child_relative), "%s/%s", relative,
                            item->d_name) >= (int)sizeof(child_relative)) goto fail;
        if (snprintf(child_full, sizeof(child_full), "%s/%s", root, child_relative)
            >= (int)sizeof(child_full)) goto fail;
        if (lstat(child_full, &metadata) != 0 || S_ISLNK(metadata.st_mode))
            goto fail;
        if (S_ISDIR(metadata.st_mode)) {
            if (scan_directory(root, child_relative, entries, count) != 0)
                goto fail;
        } else if (S_ISREG(metadata.st_mode)) {
            if (strcmp(child_relative, INVENTORY_NAME) == 0
                || strcmp(child_relative, LAUNCHER_NAME) == 0)
                continue;
            if (verify_regular_file(child_full, &metadata,
                                    find_entry(entries, count, child_relative)) != 0)
                goto fail;
        } else {
            goto fail;
        }
    }
    if (closedir(directory) != 0)
        return -1;
    return 0;
fail:
    closedir(directory);
    return -1;
}

static int executable_directory(char output[PATH_MAX]) {
    ssize_t length = readlink("/proc/self/exe", output, PATH_MAX - 1);
    char *separator;
    if (length <= 0 || length >= PATH_MAX - 1)
        return -1;
    output[length] = '\0';
    separator = strrchr(output, '/');
    if (separator == NULL || separator == output)
        return -1;
    *separator = '\0';
    return 0;
}

int main(int argc, char **argv) {
    char root[PATH_MAX], inventory_path[PATH_MAX], runtime_path[PATH_MAX];
    uint8_t inventory_digest[32];
    char inventory_hex[65];
    inventory_entry *entries = NULL;
    size_t count = 0, index;
    (void)argc;
    if (strspn((const char *)expected_inventory_sha256, "0") == 64) {
        fprintf(stderr, "cfetch OpenVINO launcher has no package inventory binding\n");
        return 126;
    }
    if (executable_directory(root) != 0
        || snprintf(inventory_path, sizeof(inventory_path), "%s/%s", root,
                    INVENTORY_NAME) >= (int)sizeof(inventory_path)
        || snprintf(runtime_path, sizeof(runtime_path), "%s/%s", root,
                    RUNTIME_NAME) >= (int)sizeof(runtime_path)
        || hash_file(inventory_path, inventory_digest) != 0) {
        fprintf(stderr, "cfetch OpenVINO launcher could not inspect its package inventory\n");
        return 126;
    }
    digest_hex(inventory_digest, inventory_hex);
    if (strcmp(inventory_hex, (const char *)expected_inventory_sha256) != 0
        || load_inventory(inventory_path, &entries, &count) != 0
        || scan_directory(root, "", entries, count) != 0) {
        fprintf(stderr, "cfetch OpenVINO package inventory verification failed\n");
        free_entries(entries, count);
        return 126;
    }
    for (index = 0; index < count; ++index) {
        if (!entries[index].seen) {
            fprintf(stderr, "cfetch OpenVINO package inventory is incomplete\n");
            free_entries(entries, count);
            return 126;
        }
    }
    free_entries(entries, count);
    if (setenv("CFETCH_PACKAGE_INVENTORY_SHA256",
               (const char *)expected_inventory_sha256, 1) != 0) {
        fprintf(stderr, "cfetch OpenVINO launcher could not bind its verified inventory\n");
        return 126;
    }
    execv(runtime_path, argv);
    fprintf(stderr, "cfetch OpenVINO launcher could not start its frozen runtime\n");
    return 126;
}
