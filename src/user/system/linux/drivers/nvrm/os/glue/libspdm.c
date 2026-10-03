/*
 * libspdm's cryptography, which RM imports for Confidential Computing and
 * SPDM attestation (Hopper and later): 71 functions, each a loud stub
 * (docs/NVIDIA.md §4.2: "Stubs: ... libspdm"). A GeForce runs neither, so
 * RM reaches none of them on the 3060; each answers as NVIDIA's own
 * kernel-open glue answers on a Linux without the kernel crypto API --
 * false, or nothing -- and says it was called.
 *
 * Prototypes are libspdm's (src/nvidia/src/libraries/libspdm, BSD-3-Clause,
 * included from the fetched tree, not copied) and NVIDIA's
 * nvspdm_cryptlib_extensions.h; the bodies are Ferrix's. Generated once
 * from those headers and kept by hand.
 *
 * SPDX-License-Identifier: MIT
 */
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "library/cryptlib.h"
#include "nvspdm_cryptlib_extensions.h"

void nvos_stub_called(const char *name, const char *why);

#define SPDM_WHY "libspdm: Confidential Computing and SPDM are stubbed on a GeForce (docs/NVIDIA.md §4.2)"

bool libspdm_aead_aes_gcm_decrypt(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3, const uint8_t *a4, size_t a5, const uint8_t *a6, size_t a7, const uint8_t *a8, size_t a9, uint8_t *a10, size_t *a11)
{
    nvos_stub_called("libspdm_aead_aes_gcm_decrypt", SPDM_WHY);
    return false;
}

bool libspdm_aead_aes_gcm_decrypt_prealloc(void *a0, const uint8_t *a1, size_t a2, const uint8_t *a3, size_t a4, const uint8_t *a5, size_t a6, const uint8_t *a7, size_t a8, const uint8_t *a9, size_t a10, uint8_t *a11, size_t *a12)
{
    nvos_stub_called("libspdm_aead_aes_gcm_decrypt_prealloc", SPDM_WHY);
    return false;
}

bool libspdm_aead_aes_gcm_encrypt(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3, const uint8_t *a4, size_t a5, const uint8_t *a6, size_t a7, uint8_t *a8, size_t a9, uint8_t *a10, size_t *a11)
{
    nvos_stub_called("libspdm_aead_aes_gcm_encrypt", SPDM_WHY);
    return false;
}

bool libspdm_aead_aes_gcm_encrypt_prealloc(void *a0, const uint8_t *a1, size_t a2, const uint8_t *a3, size_t a4, const uint8_t *a5, size_t a6, const uint8_t *a7, size_t a8, uint8_t *a9, size_t a10, uint8_t *a11, size_t *a12)
{
    nvos_stub_called("libspdm_aead_aes_gcm_encrypt_prealloc", SPDM_WHY);
    return false;
}

void libspdm_aead_free(void *a0)
{
    nvos_stub_called("libspdm_aead_free", SPDM_WHY);
}

bool libspdm_aead_gcm_prealloc(void **a0)
{
    nvos_stub_called("libspdm_aead_gcm_prealloc", SPDM_WHY);
    return false;
}

bool libspdm_asn1_get_tag(uint8_t **a0, const uint8_t *a1, size_t *a2, uint32_t a3)
{
    nvos_stub_called("libspdm_asn1_get_tag", SPDM_WHY);
    return false;
}

bool libspdm_check_crypto_backend(void)
{
    nvos_stub_called("libspdm_check_crypto_backend", SPDM_WHY);
    return false;
}

bool libspdm_decode_base64(const uint8_t *a0, uint8_t *a1, size_t a2, size_t *a3)
{
    nvos_stub_called("libspdm_decode_base64", SPDM_WHY);
    return false;
}

bool libspdm_ec_compute_key(void *a0, const uint8_t *a1, size_t a2, uint8_t *a3, size_t *a4)
{
    nvos_stub_called("libspdm_ec_compute_key", SPDM_WHY);
    return false;
}

bool libspdm_ecdsa_sign(void *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4, size_t *a5)
{
    nvos_stub_called("libspdm_ecdsa_sign", SPDM_WHY);
    return false;
}

bool libspdm_ecdsa_verify(void *a0, size_t a1, const uint8_t *a2, size_t a3, const uint8_t *a4, size_t a5)
{
    nvos_stub_called("libspdm_ecdsa_verify", SPDM_WHY);
    return false;
}

void libspdm_ec_free(void *a0)
{
    nvos_stub_called("libspdm_ec_free", SPDM_WHY);
}

bool libspdm_ec_generate_key(void *a0, uint8_t *a1, size_t *a2)
{
    nvos_stub_called("libspdm_ec_generate_key", SPDM_WHY);
    return false;
}

bool libspdm_ec_get_public_key_from_x509(const uint8_t *a0, size_t a1, void **a2)
{
    nvos_stub_called("libspdm_ec_get_public_key_from_x509", SPDM_WHY);
    return false;
}

void * libspdm_ec_new_by_nid(size_t a0)
{
    nvos_stub_called("libspdm_ec_new_by_nid", SPDM_WHY);
    return NULL;
}

bool libspdm_encode_base64(const uint8_t *a0, uint8_t *a1, size_t a2, size_t *a3)
{
    nvos_stub_called("libspdm_encode_base64", SPDM_WHY);
    return false;
}

bool libspdm_hkdf_sha256_expand(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4, size_t a5)
{
    nvos_stub_called("libspdm_hkdf_sha256_expand", SPDM_WHY);
    return false;
}

bool libspdm_hkdf_sha256_extract(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4, size_t a5)
{
    nvos_stub_called("libspdm_hkdf_sha256_extract", SPDM_WHY);
    return false;
}

bool libspdm_hkdf_sha384_expand(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4, size_t a5)
{
    nvos_stub_called("libspdm_hkdf_sha384_expand", SPDM_WHY);
    return false;
}

bool libspdm_hkdf_sha384_extract(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4, size_t a5)
{
    nvos_stub_called("libspdm_hkdf_sha384_extract", SPDM_WHY);
    return false;
}

bool libspdm_hmac_sha256_all(const void *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4)
{
    nvos_stub_called("libspdm_hmac_sha256_all", SPDM_WHY);
    return false;
}

bool libspdm_hmac_sha256_duplicate(const void *a0, void *a1)
{
    nvos_stub_called("libspdm_hmac_sha256_duplicate", SPDM_WHY);
    return false;
}

bool libspdm_hmac_sha256_final(void *a0, uint8_t *a1)
{
    nvos_stub_called("libspdm_hmac_sha256_final", SPDM_WHY);
    return false;
}

void libspdm_hmac_sha256_free(void *a0)
{
    nvos_stub_called("libspdm_hmac_sha256_free", SPDM_WHY);
}

void * libspdm_hmac_sha256_new(void)
{
    nvos_stub_called("libspdm_hmac_sha256_new", SPDM_WHY);
    return NULL;
}

bool libspdm_hmac_sha256_set_key(void *a0, const uint8_t *a1, size_t a2)
{
    nvos_stub_called("libspdm_hmac_sha256_set_key", SPDM_WHY);
    return false;
}

bool libspdm_hmac_sha256_update(void *a0, const void *a1, size_t a2)
{
    nvos_stub_called("libspdm_hmac_sha256_update", SPDM_WHY);
    return false;
}

bool libspdm_hmac_sha384_all(const void *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4)
{
    nvos_stub_called("libspdm_hmac_sha384_all", SPDM_WHY);
    return false;
}

bool libspdm_hmac_sha384_duplicate(const void *a0, void *a1)
{
    nvos_stub_called("libspdm_hmac_sha384_duplicate", SPDM_WHY);
    return false;
}

bool libspdm_hmac_sha384_final(void *a0, uint8_t *a1)
{
    nvos_stub_called("libspdm_hmac_sha384_final", SPDM_WHY);
    return false;
}

void libspdm_hmac_sha384_free(void *a0)
{
    nvos_stub_called("libspdm_hmac_sha384_free", SPDM_WHY);
}

void * libspdm_hmac_sha384_new(void)
{
    nvos_stub_called("libspdm_hmac_sha384_new", SPDM_WHY);
    return NULL;
}

bool libspdm_hmac_sha384_set_key(void *a0, const uint8_t *a1, size_t a2)
{
    nvos_stub_called("libspdm_hmac_sha384_set_key", SPDM_WHY);
    return false;
}

bool libspdm_hmac_sha384_update(void *a0, const void *a1, size_t a2)
{
    nvos_stub_called("libspdm_hmac_sha384_update", SPDM_WHY);
    return false;
}

bool libspdm_random_bytes(uint8_t *a0, size_t a1)
{
    nvos_stub_called("libspdm_random_bytes", SPDM_WHY);
    return false;
}

void libspdm_rsa_free(void *a0)
{
    nvos_stub_called("libspdm_rsa_free", SPDM_WHY);
}

bool libspdm_rsa_get_public_key_from_x509(const uint8_t *a0, size_t a1, void **a2)
{
    nvos_stub_called("libspdm_rsa_get_public_key_from_x509", SPDM_WHY);
    return false;
}

void * libspdm_rsa_new(void)
{
    nvos_stub_called("libspdm_rsa_new", SPDM_WHY);
    return NULL;
}

bool libspdm_rsa_pss_sign(void *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4, size_t *a5)
{
    nvos_stub_called("libspdm_rsa_pss_sign", SPDM_WHY);
    return false;
}

bool libspdm_rsa_pss_verify(void *a0, size_t a1, const uint8_t *a2, size_t a3, const uint8_t *a4, size_t a5)
{
    nvos_stub_called("libspdm_rsa_pss_verify", SPDM_WHY);
    return false;
}

bool libspdm_rsa_set_key(void *a0, const libspdm_rsa_key_tag_t a1, const uint8_t *a2, size_t a3)
{
    nvos_stub_called("libspdm_rsa_set_key", SPDM_WHY);
    return false;
}

bool libspdm_sha256_duplicate(const void *a0, void *a1)
{
    nvos_stub_called("libspdm_sha256_duplicate", SPDM_WHY);
    return false;
}

bool libspdm_sha256_final(void *a0, uint8_t *a1)
{
    nvos_stub_called("libspdm_sha256_final", SPDM_WHY);
    return false;
}

void libspdm_sha256_free(void *a0)
{
    nvos_stub_called("libspdm_sha256_free", SPDM_WHY);
}

bool libspdm_sha256_hash_all(const void *a0, size_t a1, uint8_t *a2)
{
    nvos_stub_called("libspdm_sha256_hash_all", SPDM_WHY);
    return false;
}

bool libspdm_sha256_init(void *a0)
{
    nvos_stub_called("libspdm_sha256_init", SPDM_WHY);
    return false;
}

void * libspdm_sha256_new(void)
{
    nvos_stub_called("libspdm_sha256_new", SPDM_WHY);
    return NULL;
}

bool libspdm_sha256_update(void *a0, const void *a1, size_t a2)
{
    nvos_stub_called("libspdm_sha256_update", SPDM_WHY);
    return false;
}

bool libspdm_sha384_duplicate(const void *a0, void *a1)
{
    nvos_stub_called("libspdm_sha384_duplicate", SPDM_WHY);
    return false;
}

bool libspdm_sha384_final(void *a0, uint8_t *a1)
{
    nvos_stub_called("libspdm_sha384_final", SPDM_WHY);
    return false;
}

void libspdm_sha384_free(void *a0)
{
    nvos_stub_called("libspdm_sha384_free", SPDM_WHY);
}

bool libspdm_sha384_hash_all(const void *a0, size_t a1, uint8_t *a2)
{
    nvos_stub_called("libspdm_sha384_hash_all", SPDM_WHY);
    return false;
}

bool libspdm_sha384_init(void *a0)
{
    nvos_stub_called("libspdm_sha384_init", SPDM_WHY);
    return false;
}

void * libspdm_sha384_new(void)
{
    nvos_stub_called("libspdm_sha384_new", SPDM_WHY);
    return NULL;
}

bool libspdm_sha384_update(void *a0, const void *a1, size_t a2)
{
    nvos_stub_called("libspdm_sha384_update", SPDM_WHY);
    return false;
}

int32_t libspdm_x509_compare_date_time(const void *a0, const void *a1)
{
    nvos_stub_called("libspdm_x509_compare_date_time", SPDM_WHY);
    return 0;
}

bool libspdm_x509_get_cert_from_cert_chain(const uint8_t *a0, size_t a1, const int32_t a2, const uint8_t **a3, size_t *a4)
{
    nvos_stub_called("libspdm_x509_get_cert_from_cert_chain", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_extended_basic_constraints(const uint8_t *a0, size_t a1, uint8_t *a2, size_t *a3)
{
    nvos_stub_called("libspdm_x509_get_extended_basic_constraints", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_extended_key_usage(const uint8_t *a0, size_t a1, uint8_t *a2, size_t *a3)
{
    nvos_stub_called("libspdm_x509_get_extended_key_usage", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_extension_data(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3, uint8_t *a4, size_t *a5)
{
    nvos_stub_called("libspdm_x509_get_extension_data", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_issuer_name(const uint8_t *a0, size_t a1, uint8_t *a2, size_t *a3)
{
    nvos_stub_called("libspdm_x509_get_issuer_name", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_key_usage(const uint8_t *a0, size_t a1, size_t *a2)
{
    nvos_stub_called("libspdm_x509_get_key_usage", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_serial_number(const uint8_t *a0, size_t a1, uint8_t *a2, size_t *a3)
{
    nvos_stub_called("libspdm_x509_get_serial_number", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_signature_algorithm(const uint8_t *a0, size_t a1, uint8_t *a2, size_t *a3)
{
    nvos_stub_called("libspdm_x509_get_signature_algorithm", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_subject_name(const uint8_t *a0, size_t a1, uint8_t *a2, size_t *a3)
{
    nvos_stub_called("libspdm_x509_get_subject_name", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_validity(const uint8_t *a0, size_t a1, uint8_t *a2, size_t *a3, uint8_t *a4, size_t *a5)
{
    nvos_stub_called("libspdm_x509_get_validity", SPDM_WHY);
    return false;
}

bool libspdm_x509_get_version(const uint8_t *a0, size_t a1, size_t *a2)
{
    nvos_stub_called("libspdm_x509_get_version", SPDM_WHY);
    return false;
}

bool libspdm_x509_set_date_time(const char *a0, void *a1, size_t *a2)
{
    nvos_stub_called("libspdm_x509_set_date_time", SPDM_WHY);
    return false;
}

bool libspdm_x509_verify_cert(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3)
{
    nvos_stub_called("libspdm_x509_verify_cert", SPDM_WHY);
    return false;
}

bool libspdm_x509_verify_cert_chain(const uint8_t *a0, size_t a1, const uint8_t *a2, size_t a3)
{
    nvos_stub_called("libspdm_x509_verify_cert_chain", SPDM_WHY);
    return false;
}

