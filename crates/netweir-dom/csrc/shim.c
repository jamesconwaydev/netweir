/*
 * Accessors over lexbor's DOM and selector engine.
 *
 * The Rust side calls only these functions, so it never depends on the
 * layout of lexbor's structs, which change between lexbor releases.
 */
#include <lexbor/css/css.h>
#include <lexbor/dom/dom.h>
#include <lexbor/html/html.h>
#include <lexbor/selectors/selectors.h>

/* ---- documents ---------------------------------------------------------- */

/*
 * Chunked parsing, so the caller can stop between chunks. Returns NULL (begin)
 * or non-zero (chunk, end) when lexbor cannot allocate.
 */
lxb_html_document_t *nw_chunk_begin(void) {
    lxb_html_document_t *doc = lxb_html_document_create();
    if (doc == NULL) {
        return NULL;
    }
    if (lxb_html_document_parse_chunk_begin(doc) != LXB_STATUS_OK) {
        lxb_html_document_destroy(doc);
        return NULL;
    }
    return doc;
}

int nw_chunk(lxb_html_document_t *doc, const char *html, size_t len) {
    return lxb_html_document_parse_chunk(doc, (const lxb_char_t *) html, len) == LXB_STATUS_OK ? 0 : -1;
}

int nw_chunk_end(lxb_html_document_t *doc) {
    return lxb_html_document_parse_chunk_end(doc) == LXB_STATUS_OK ? 0 : -1;
}

void nw_destroy(lxb_html_document_t *doc) {
    lxb_html_document_destroy(doc);
}

lxb_dom_node_t *nw_root(lxb_html_document_t *doc) {
    return lxb_dom_interface_node(doc);
}

/* ---- nodes -------------------------------------------------------------- */

lxb_dom_node_t *nw_parent(lxb_dom_node_t *n) { return n->parent; }
lxb_dom_node_t *nw_first_child(lxb_dom_node_t *n) { return n->first_child; }
lxb_dom_node_t *nw_last_child(lxb_dom_node_t *n) { return n->last_child; }
lxb_dom_node_t *nw_next(lxb_dom_node_t *n) { return n->next; }
lxb_dom_node_t *nw_prev(lxb_dom_node_t *n) { return n->prev; }
int nw_type(lxb_dom_node_t *n) { return (int) n->type; }

/* Tag ids: equal ids mean equal (lowercased) names. 0 is "no such tag". */
uintptr_t nw_tag_id(lxb_dom_node_t *n) { return n->local_name; }
uintptr_t nw_tag_id_named(lxb_dom_node_t *n, const char *name, size_t len) {
    return lxb_tag_id_by_name(n->owner_document->tags, (const lxb_char_t *) name, len);
}

const char *nw_tag(lxb_dom_node_t *n, size_t *len) {
    return (const char *) lxb_dom_element_local_name(lxb_dom_interface_element(n), len);
}

/* Text and comment nodes. */
const char *nw_char_data(lxb_dom_node_t *n, size_t *len) {
    lxb_dom_character_data_t *cd = lxb_dom_interface_character_data(n);
    *len = cd->data.length;
    return (const char *) cd->data.data;
}

/* NULL when the attribute is absent; "" when present without a value. */
const char *nw_get_attr(lxb_dom_node_t *n, const char *name, size_t name_len, size_t *len) {
    lxb_dom_attr_t *attr = lxb_dom_element_attr_by_name(
        lxb_dom_interface_element(n), (const lxb_char_t *) name, name_len);
    if (attr == NULL) {
        return NULL;
    }
    const lxb_char_t *value = lxb_dom_attr_value(attr, len);
    if (value == NULL) {
        *len = 0;
        return "";
    }
    return (const char *) value;
}

lxb_dom_attr_t *nw_first_attr(lxb_dom_node_t *n) {
    return lxb_dom_element_first_attribute(lxb_dom_interface_element(n));
}

lxb_dom_attr_t *nw_next_attr(lxb_dom_attr_t *a) {
    return lxb_dom_element_next_attribute(a);
}

const char *nw_attr_name(lxb_dom_attr_t *a, size_t *len) {
    return (const char *) lxb_dom_attr_local_name(a, len);
}

const char *nw_attr_value(lxb_dom_attr_t *a, size_t *len) {
    const lxb_char_t *v = lxb_dom_attr_value(a, len);
    if (v == NULL) {
        *len = 0;
        return "";
    }
    return (const char *) v;
}

/* ---- whole-tree walks --------------------------------------------------- */

/* The node after `n` in document order, staying inside `root`. */
static lxb_dom_node_t *nw_step(lxb_dom_node_t *n, lxb_dom_node_t *root) {
    if (n->first_child != NULL) {
        return n->first_child;
    }
    while (n != root) {
        if (n->next != NULL) {
            return n->next;
        }
        n = n->parent;
    }
    return NULL;
}

/* Numbers every node under `root` (root first) in document order, keeping
 * the number in the node's `user` field, which lexbor leaves to callers.
 * Returns how many nodes there are. */
size_t nw_number(lxb_dom_node_t *root) {
    uintptr_t i = 0;
    for (lxb_dom_node_t *n = root; n != NULL; n = nw_step(n, root)) {
        n->user = (void *) i++;
    }
    return (size_t) i;
}

uintptr_t nw_order(lxb_dom_node_t *n) { return (uintptr_t) n->user; }

/* After nw_number: fills one entry per node, in document order, with the
 * node, its parent's number (UINT32_MAX for the root), its type and its tag
 * id. Each array holds as many entries as nw_number returned. */
void nw_index_fill(lxb_dom_node_t *root, lxb_dom_node_t **raw, uint32_t *parent,
                   uint8_t *type, uintptr_t *tag) {
    size_t i = 0;
    for (lxb_dom_node_t *n = root; n != NULL; n = nw_step(n, root), i++) {
        raw[i] = n;
        parent[i] = (n == root || n->parent == NULL) ? UINT32_MAX : (uint32_t) (uintptr_t) n->parent->user;
        type[i] = (uint8_t) n->type;
        tag[i] = n->local_name;
    }
}

/* ---- serialising ------------------------------------------------------- */

typedef void (*nw_chunk_fn)(const char *data, size_t len, void *ctx);

typedef struct {
    nw_chunk_fn chunk;
    void *ctx;
} nw_sink_t;

static lxb_status_t nw_on_chunk(const lxb_char_t *data, size_t len, void *ctx) {
    nw_sink_t *sink = ctx;
    sink->chunk((const char *) data, len, sink->ctx);
    return LXB_STATUS_OK;
}

/* Serialises `n` and everything below it as HTML, in pieces. Returns 0 on
 * success. Reads the tree only. */
int nw_serialize(lxb_dom_node_t *n, nw_chunk_fn chunk, void *ctx) {
    nw_sink_t sink = {chunk, ctx};
    return lxb_html_serialize_tree_cb(n, nw_on_chunk, &sink) == LXB_STATUS_OK ? 0 : -1;
}

/* ---- CSS ---------------------------------------------------------------- */

/* The returned list owns its memory and outlives the parser that made it. */
lxb_css_selector_list_t *nw_css_compile(const char *css, size_t len) {
    lxb_css_parser_t *parser = lxb_css_parser_create();
    if (lxb_css_parser_init(parser, NULL) != LXB_STATUS_OK) {
        lxb_css_parser_destroy(parser, true);
        return NULL;
    }
    lxb_css_selector_list_t *list =
        lxb_css_selectors_parse(parser, (const lxb_char_t *) css, len);
    if (parser->status != LXB_STATUS_OK) {
        if (list != NULL) {
            lxb_css_selector_list_destroy_memory(list);
        }
        list = NULL;
    }
    lxb_css_parser_destroy(parser, true);
    return list;
}

void nw_css_free(lxb_css_selector_list_t *list) {
    lxb_css_selector_list_destroy_memory(list);
}

typedef void (*nw_found_fn)(lxb_dom_node_t *node, void *ctx);

typedef struct {
    nw_found_fn found;
    void *ctx;
} nw_found_t;

static lxb_status_t nw_on_match(lxb_dom_node_t *node, lxb_css_selector_specificity_t spec,
                                void *ctx) {
    nw_found_t *f = ctx;
    f->found(node, f->ctx);
    return LXB_STATUS_OK;
}

/*
 * Calls `found` once per element below `root` (not `root` itself) that matches
 * any selector in `list`, in document order. Returns 0 on success.
 */
int nw_select(lxb_dom_node_t *root, lxb_css_selector_list_t *list, nw_found_fn found, void *ctx) {
    lxb_selectors_t *sel = lxb_selectors_create();
    if (lxb_selectors_init(sel) != LXB_STATUS_OK) {
        lxb_selectors_destroy(sel, true);
        return -1;
    }
    lxb_selectors_opt_set(sel, LXB_SELECTORS_OPT_MATCH_FIRST);
    nw_found_t f = {found, ctx};
    lxb_status_t status = lxb_selectors_find(sel, root, list, nw_on_match, &f);
    lxb_selectors_destroy(sel, true);
    return status == LXB_STATUS_OK ? 0 : -1;
}
