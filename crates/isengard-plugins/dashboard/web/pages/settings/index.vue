<template>
  <AppShell>
    <PageHeader title="Settings" subtitle="Controller configuration" />
    <div class="flex-1 overflow-y-auto">
      <div class="max-w-3xl mx-auto w-full p-6">
        <FleetsSettings />
        <NotifierSettings />
        <EnrollmentSettings />
      </div>
    </div>
  </AppShell>
</template>
