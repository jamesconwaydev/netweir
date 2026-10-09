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
lxb_dom_node_t *nw_next(lxb_dom_node_t *n) { return n->next; }
int nw_type(lxb_dom_node_t *n) { return (int) n->type; }

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
