idf_component_register(
    SRCS "vadnet_bridge.c"
    INCLUDE_DIRS "include"
    REQUIRES espressif__esp-sr
)

# esp-dl 3.3.x uses MALLOC_CAP_SIMD, which IDF 5.4 does not define.
idf_component_get_property(espdl_lib espressif__esp-dl COMPONENT_LIB)
if(espdl_lib)
    target_compile_definitions(${espdl_lib} PRIVATE MALLOC_CAP_SIMD=0)
endif()
