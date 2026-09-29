/* NeuraScribeGrit — C ABI for the grit-backed Git backend.
 *
 * Hand-written rather than generated: the shim exports 26 functions, and
 * cbindgen would add a build step (and a version pin) to save 60 lines.
 * Keep this in step with `src/lib.rs` in ../ (the Rust crate).
 */

#ifndef NEURASCRIBE_GRIT_H
#define NEURASCRIBE_GRIT_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque repository handle. */
typedef struct GritRepoHandle GritRepoHandle;

/* One HTTP response handed back from the embedder.
 *
 * Every pointer stays valid until `grit_http_release` is called on the same
 * struct. The status is passed through rather than folded into an error so
 * the shim can run the 401-retry dance itself. */
typedef struct {
    int32_t status;            /* 0 when the request never completed */
    const uint8_t *body;       /* may be NULL when body_len is 0 */
    size_t body_len;
    const char *final_url;     /* NULL when unknown */
    const char *error;         /* NULL on success */
    void *context;            /* opaque, returned verbatim to release */
} GritHttpResponse;

/* HTTP verbs the shim calls back into Swift for. `auth` is a ready-made
 * `Authorization` header value, or NULL. */
typedef GritHttpResponse (*GritHttpGetFn)(const char *url, const char *auth, const char *git_protocol);
typedef GritHttpResponse (*GritHttpPostFn)(const char *url, const char *auth, const char *content_type,
                                          const char *accept, const uint8_t *body, size_t body_len,
                                          const char *git_protocol);
typedef void (*GritHttpReleaseFn)(GritHttpResponse *response);

/* Install the HTTP bridge. Pass NULL to clear it, which makes the next
 * network call fail loudly instead of looking like a clean fetch. */
void grit_http_set_handlers(GritHttpGetFn get, GritHttpPostFn post, GritHttpReleaseFn release);

/* Last error recorded on the calling thread. Never NULL; empty when there is
 * none. Release with grit_string_free. */
char *grit_last_error(void);
void grit_string_free(char *s);

GritRepoHandle *grit_repo_open(const char *path);
void grit_repo_free(GritRepoHandle *repo);
/* 1 when path is a repository, 0 otherwise (never an error). */
int32_t grit_repo_exists(const char *path);

char *grit_repo_status_json(GritRepoHandle *repo);
char *grit_repo_current_branch(GritRepoHandle *repo);
char *grit_repo_head_sha(GritRepoHandle *repo);
/* 0 on success, -1 on failure (message in grit_last_error). */
int32_t grit_repo_stage_all(GritRepoHandle *repo);
char *grit_repo_commit(GritRepoHandle *repo, const char *message, const char *author_name, const char *author_email);
char *grit_repo_diff_file(GritRepoHandle *repo, const char *path);
char *grit_repo_diff_commit_json(GritRepoHandle *repo, const char *sha);
/* Commits reachable from HEAD, newest first, by AUTHOR date (git
 * --author-date-order). Returns JSON:
 *   [{"hash":…,"author_time":…,"subject":…,"message":…}]
 * author_time is epoch seconds. Free with grit_string_free. */
char *grit_repo_log_json(GritRepoHandle *repo, size_t limit);
char *grit_repo_changed_files_json(GritRepoHandle *repo, const char *base, const char *head);
/* Resolve a ref name (or a raw SHA) to a commit SHA, as a JSON string.
 * Reads packed-refs as well as loose refs. Free with grit_string_free. */
char *grit_repo_resolve_ref(GritRepoHandle *repo, const char *refname);

/* Every tracked path at a commit, as a JSON array of repo-relative sorted
 * paths (git ls-tree -r). Free with grit_string_free. */
char *grit_repo_list_files_json(GritRepoHandle *repo, const char *sha);

/* Stage one path, including recording a deletion (git add -- <path>).
 * 0 on success, -1 on failure. */
int32_t grit_repo_stage_file(GritRepoHandle *repo, const char *path);

/* Fast-forward HEAD and the work tree to a commit; new HEAD as a JSON string.
 * Fails when the histories have diverged. Free with grit_string_free. */
char *grit_repo_fast_forward_json(GritRepoHandle *repo, const char *tip);
/* 0 on success, -1 on failure. */
int32_t grit_repo_reset_file_to_head(GritRepoHandle *repo, const char *path);
char *grit_repo_reset_hard_to(GritRepoHandle *repo, const char *sha);
char *grit_repo_force_checkout_head(GritRepoHandle *repo);
char *grit_repo_list_remotes_json(GritRepoHandle *repo);
int64_t grit_repo_ahead_count(GritRepoHandle *repo, const char *remote_tip);
/* 1 yes, 0 no, -1 error (see grit_last_error). */
int32_t grit_repo_is_ancestor(GritRepoHandle *repo, const char *a, const char *b);
/* NULL when unrelated or on failure; check grit_last_error to tell them apart. */
char *grit_repo_merge_base(GritRepoHandle *repo, const char *a, const char *b);
char *grit_repo_blob_text(GritRepoHandle *repo, const char *sha, const char *path);
char *grit_repo_work_tree(GritRepoHandle *repo);

/* Network. `credential` is optional JSON: {"username":"…","password":"…"}. */
char *grit_clone(const char *url, const char *dest, const char *branch, const char *credential);
char *grit_repo_fetch_json(GritRepoHandle *repo, const char *url, const char *refspecs_json, const char *credential);
char *grit_repo_push_json(GritRepoHandle *repo, const char *url, const char *branch, int32_t force, const char *credential);

/* Submodules. `grit_repo_submodules_json` returns an array of
   {name, path, url, indexed_sha, head_sha, initialized}; the two shas are null
   and `initialized` is false when the submodule is not checked out. */
char *grit_repo_submodules_json(GritRepoHandle *repo);
/* 0 on success, -1 on failure. The clone must already exist. */
int32_t grit_repo_record_submodule(GritRepoHandle *repo, const char *name, const char *path, const char *url, const char *sha);
/* Drops the registration, the gitlink, the work tree and .git/modules/<name>. */
int32_t grit_repo_remove_submodule(GritRepoHandle *repo, const char *path);
/* Checks the submodule's work tree out at the superproject's recorded commit. */
char *grit_repo_checkout_submodule_to(GritRepoHandle *repo, const char *path, const char *sha);

#ifdef __cplusplus
}
#endif

#endif /* NEURASCRIBE_GRIT_H */

/// Paths the index tracks below a directory, as a JSON array of strings.
/// Returns NULL on failure; free with grit_string_free.
char *grit_repo_indexed_paths_under_json(GritRepoHandle *repo, const char *directory);

/// Stop tracking everything under a directory, leaving the files on disk
/// (git rm -r --cached). Returns entries removed, or -1 on failure.
int32_t grit_repo_untrack_directory(GritRepoHandle *repo, const char *directory);