/*
 * Singly-linked list implementation.
 * Exercises: structs, typedefs, pointers, malloc/free, switch, while, for.
 */

#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

/* Node kinds */
#define NODE_INT  0
#define NODE_STR  1
#define NODE_NONE 2

typedef enum {
    LIST_OK = 0,
    LIST_ERR_ALLOC,
    LIST_ERR_NOT_FOUND,
    LIST_ERR_INVALID,
} ListResult;

typedef struct Node {
    int kind;
    union {
        int   ival;
        char *sval;
    } data;
    struct Node *next;
} Node;

typedef struct {
    Node  *head;
    size_t len;
} List;

/* Allocate a new integer node. */
static Node *node_int(int v) {
    Node *n = malloc(sizeof(Node));
    if (!n) return NULL;
    n->kind      = NODE_INT;
    n->data.ival = v;
    n->next      = NULL;
    return n;
}

/* Allocate a new string node (takes ownership of s). */
static Node *node_str(char *s) {
    Node *n = malloc(sizeof(Node));
    if (!n) return NULL;
    n->kind      = NODE_STR;
    n->data.sval = s;
    n->next      = NULL;
    return n;
}

void list_init(List *l) {
    l->head = NULL;
    l->len  = 0;
}

ListResult list_push_int(List *l, int v) {
    Node *n = node_int(v);
    if (!n) return LIST_ERR_ALLOC;
    n->next = l->head;
    l->head = n;
    l->len++;
    return LIST_OK;
}

ListResult list_push_str(List *l, const char *s) {
    char *copy = strdup(s);
    if (!copy) return LIST_ERR_ALLOC;
    Node *n = node_str(copy);
    if (!n) {
        free(copy);
        return LIST_ERR_ALLOC;
    }
    n->next = l->head;
    l->head = n;
    l->len++;
    return LIST_OK;
}

/* Print the list to stdout. */
void list_print(const List *l) {
    const Node *cur = l->head;
    size_t i = 0;
    while (cur) {
        switch (cur->kind) {
        case NODE_INT:
            printf("[%zu] int  = %d\n", i, cur->data.ival);
            break;
        case NODE_STR:
            printf("[%zu] str  = \"%s\"\n", i, cur->data.sval);
            break;
        default:
            printf("[%zu] unknown kind %d\n", i, cur->kind);
            break;
        }
        cur = cur->next;
        i++;
    }
}

/* Free all nodes. */
void list_free(List *l) {
    Node *cur = l->head;
    while (cur) {
        Node *next = cur->next;
        if (cur->kind == NODE_STR) {
            free(cur->data.sval);
        }
        free(cur);
        cur = next;
    }
    l->head = NULL;
    l->len  = 0;
}

/* Return the nth node or NULL. */
Node *list_get(const List *l, size_t idx) {
    Node *cur = l->head;
    for (size_t i = 0; cur && i < idx; i++) {
        cur = cur->next;
    }
    return cur;
}

/* Reverse the list in place. */
void list_reverse(List *l) {
    Node *prev = NULL;
    Node *cur  = l->head;
    while (cur) {
        Node *next  = cur->next;
        cur->next   = prev;
        prev        = cur;
        cur         = next;
    }
    l->head = prev;
}

int main(void) {
    List l;
    list_init(&l);

    if (list_push_int(&l, 1)      != LIST_OK) return 1;
    if (list_push_int(&l, 2)      != LIST_OK) return 1;
    if (list_push_str(&l, "hello") != LIST_OK) return 1;
    if (list_push_str(&l, "world") != LIST_OK) return 1;

    printf("--- forward ---\n");
    list_print(&l);

    list_reverse(&l);
    printf("--- reversed ---\n");
    list_print(&l);

    list_free(&l);
    return 0;
}
