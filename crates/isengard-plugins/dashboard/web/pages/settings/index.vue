<template>
  <div class="h-screen flex flex-col">
    <TopBar />
    <div class="max-w-3xl mx-auto w-full p-6 overflow-y-auto">
      <h1 class="font-mono text-xl mb-1">Settings</h1>
      <p class="text-sm text-iso-text-muted mb-6">Controller configuration.</p>

      <FleetsSettings />
      <NotifierSettings />
      <EnrollmentSettings />
    </div>
  </div>
</template>
