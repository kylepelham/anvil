set(VCPKG_POLICY_EMPTY_PACKAGE enabled)
file(INSTALL "${CMAKE_CURRENT_LIST_DIR}/optport.h" DESTINATION "${CURRENT_PACKAGES_DIR}/include")
file(INSTALL "${CMAKE_CURRENT_LIST_DIR}/optportConfig.cmake" DESTINATION "${CURRENT_PACKAGES_DIR}/share/optport")
file(WRITE "${CURRENT_PACKAGES_DIR}/share/optport/copyright" "MIT")
